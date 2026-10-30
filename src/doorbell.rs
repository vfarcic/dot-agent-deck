//! PRD #1666 S4: the doorbell — the one line the deck still types into an
//! agent's pane.
//!
//! A message waits in the agent's inbox ([`crate::inbox_hub`]). When no
//! agent-side component is armed to take it, the deck types one line that
//! names a waiting message and the command that fetches it, led by a ring
//! token that ties the agent's prompt to this ring (PRD #1666 part 3, round
//! 5-wake, plan §1.2):
//!
//! ```text
//! (abcdfg) dot-agent-deck: message m-k3xzq2ab-0f3c9a71d2e4b850 waiting, run: dot-agent-deck inbox fetch
//! ```
//!
//! The content itself is never typed.
//!
//! **When it rings** — every condition below, re-checked on each tick:
//!
//! 1. the agent generation has at least one `queued` message (not `fetched`);
//! 2. no waiter is armed for it, and a `Native` announcement's
//!    [`DoorbellConfig::arm_grace`] has passed;
//! 3. its last hook-reported status is **Idle** — never `WaitingForInput`
//!    (dialogs and permission prompts), Thinking, Working, Compacting, Blocked,
//!    Error, or Unknown (no hook yet), nor the Idle of a Codex wrapper's
//!    fork-time start alone — and its session has not declared the pane
//!    history-only or view-only;
//! 4. the deck may type into the generation at all: its release is one the
//!    deck has measured ([`crate::doorbell_activation`]), its input mode is
//!    proven raw, it has a way to show that the line was submitted, its
//!    doorbell is not suspended on an earlier attempt, and its first ring has
//!    waited out the settle window ([`crate::doorbell_generation`]); no paste
//!    into the pane is open;
//! 5. the ring is due on the schedule ([`DoorbellConfig::schedule`]).
//!
//! A ring is one transaction ([`crate::doorbell_txn`], driven on the real
//! writer by [`AgentPtyRegistry::ring_doorbell`]): under one hold of the
//! pane's writer it admits the attempt on a Clean frame showing the agent's
//! empty ready composer, types its line, proves the agent echoed exactly that
//! line, and presses Enter only after a synchronous re-check. It ends
//! `Submitted` (the agent's prompt hook reported the token, or the transcript
//! shows the line), `Withdrawn` (the deck deleted its own line under the
//! strict recogniser), `Unresolved` (the generation's doorbell is then
//! suspended until the user's own submit or clear, and the card says so), or
//! with nothing written. An unsent draft the input tracker still remembers
//! does not stop a ring: an empty composer on screen, and the echo, are the
//! proof the draft is gone. No CR is ever sent without that echo, so nothing
//! is flushed or cleaned up afterwards.
//!
//! It is **coalesced per agent**: one ring covers every queued message, and the
//! schedule restarts only once the queue has emptied. An Enter the agent did
//! not take, withdrawn, ends the rings until the agent's next turn.
//!
//! [`decide`] is the pure policy; [`spawn`] runs it against the daemon.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::agent_pty::{AgentPtyRegistry, RingOutcome};
use crate::inbox_hub::DoorbellFacts;
use crate::state::SharedState;

/// How often the doorbell re-checks its conditions, in milliseconds.
pub const ENV_TICK_MS: &str = "DOT_AGENT_DECK_DOORBELL_TICK_MS";

/// The re-ring delays, in milliseconds, comma-separated: the first is the wait
/// after the first ring, and the last repeats.
pub const ENV_SCHEDULE_MS: &str = "DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS";

/// How long a component that announced `Native` wake-up is given to arm a
/// waiter before the doorbell rings, in milliseconds.
pub const ENV_ARM_GRACE_MS: &str = "DOT_AGENT_DECK_INBOX_ARM_GRACE_MS";

/// See [`ENV_TICK_MS`].
pub const DEFAULT_TICK: Duration = Duration::from_millis(200);

/// See [`ENV_SCHEDULE_MS`]: 15 s, 30 s, 60 s, then every 120 s.
pub const DEFAULT_SCHEDULE: [Duration; 4] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];

/// See [`ENV_ARM_GRACE_MS`].
pub const DEFAULT_ARM_GRACE: Duration = Duration::from_secs(10);

/// PR #1735 fifth review (R5): how long, in milliseconds, the oldest message
/// queued for an agent that is not working may wait before both clients show
/// its messages as waiting to wake ([`StallClock`]).
pub const ENV_WAKE_STALL_AFTER_MS: &str = "DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS";

/// See [`ENV_WAKE_STALL_AFTER_MS`]: past the first two rings of
/// [`DEFAULT_SCHEDULE`] and the native arm grace, so an ordinary wake-up —
/// native, at a turn's end, or by a doorbell the agent answers — never shows.
pub const DEFAULT_WAKE_STALL_AFTER: Duration = Duration::from_secs(120);

/// The fixed line, before its id.
pub const LINE_PREFIX: &str = " dot-agent-deck: message ";

/// The fixed line, after its id.
pub const LINE_SUFFIX: &str = " waiting, run: dot-agent-deck inbox fetch";

/// The doorbell line naming message `id`.
pub fn doorbell_line(id: &str) -> String {
    format!("{LINE_PREFIX}{id}{LINE_SUFFIX}")
}

/// The doorbell's timings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoorbellConfig {
    pub tick: Duration,
    /// Never empty.
    pub schedule: Vec<Duration>,
    pub arm_grace: Duration,
    /// See [`ENV_WAKE_STALL_AFTER_MS`].
    pub wake_stall_after: Duration,
}

impl Default for DoorbellConfig {
    fn default() -> Self {
        Self {
            tick: DEFAULT_TICK,
            schedule: DEFAULT_SCHEDULE.to_vec(),
            arm_grace: DEFAULT_ARM_GRACE,
            wake_stall_after: DEFAULT_WAKE_STALL_AFTER,
        }
    }
}

impl DoorbellConfig {
    /// The defaults, as overridden by the environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Self::from_env`] over any lookup. A value that does not parse — or a
    /// zero tick or schedule delay — is ignored with a warning.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut config = Self::default();
        if let Some(raw) = lookup(ENV_TICK_MS) {
            match parse_ms(&raw) {
                Some(ms) if !ms.is_zero() => config.tick = ms,
                _ => ignored(ENV_TICK_MS),
            }
        }
        if let Some(raw) = lookup(ENV_SCHEDULE_MS) {
            let parsed: Option<Vec<Duration>> = raw
                .split(',')
                .map(|part| parse_ms(part).filter(|d| !d.is_zero()))
                .collect();
            match parsed {
                Some(schedule) if !schedule.is_empty() => config.schedule = schedule,
                _ => ignored(ENV_SCHEDULE_MS),
            }
        }
        if let Some(raw) = lookup(ENV_ARM_GRACE_MS) {
            match parse_ms(&raw) {
                Some(ms) => config.arm_grace = ms,
                None => ignored(ENV_ARM_GRACE_MS),
            }
        }
        if let Some(raw) = lookup(ENV_WAKE_STALL_AFTER_MS) {
            match parse_ms(&raw) {
                Some(ms) if !ms.is_zero() => config.wake_stall_after = ms,
                _ => ignored(ENV_WAKE_STALL_AFTER_MS),
            }
        }
        config
    }

    /// The wait after ring number `rings` (1-based) before the next one.
    pub fn delay_after(&self, rings: u32) -> Duration {
        let index = (rings.max(1) as usize - 1).min(self.schedule.len() - 1);
        self.schedule[index]
    }
}

fn parse_ms(raw: &str) -> Option<Duration> {
    raw.trim().parse::<u64>().ok().map(Duration::from_millis)
}

fn ignored(variable: &str) {
    warn!(
        variable,
        "ignoring a doorbell override that is not a valid number of milliseconds"
    );
}

/// What the doorbell knows about one agent generation on a tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub inbox: DoorbellFacts,
    /// The last hook-reported status is Idle, and the generation is fit to
    /// ring by the session's own rules (plan §1 of PRD #1666 S5).
    pub idle: bool,
    /// Something holds the ring this tick whatever the schedule: a paste is
    /// open, the deck may not type into the generation, its first ring is
    /// settling, its doorbell is suspended, or the agent did not take the
    /// deck's last Enter in this episode.
    pub held: bool,
}

/// The doorbell's memory of one agent generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bell {
    /// Rings since the queue last emptied.
    pub rings: u32,
    /// When the last ring finished.
    pub last_ring: Option<Instant>,
    /// The messages queued when the last ring was decided.
    pub covered: Vec<String>,
}

/// What the doorbell should do for one agent generation now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Ring for the message `id`: one transaction.
    Ring {
        id: String,
    },
}

impl Bell {
    /// Fold this tick's inbox state in: an empty queue restarts the
    /// schedule, and so does a queue none of whose messages the last ring
    /// covered. A ring is confirmed only once the agent has taken it, which
    /// can be after the agent fetched every message it named; mail that
    /// arrived since is not that ring's to re-ring on its schedule.
    pub fn observe(&mut self, facts: &Facts) {
        let queued = &facts.inbox.queued;
        if queued.is_empty() || !self.covered.iter().any(|id| queued.contains(id)) {
            self.rings = 0;
            self.last_ring = None;
            self.covered.clear();
        }
    }

    /// Record a ring that wrote into the pane, decided while `covered` was
    /// queued.
    pub fn rang(&mut self, now: Instant, covered: Vec<String>) {
        self.rings = self.rings.saturating_add(1);
        self.last_ring = Some(now);
        self.covered = covered;
    }

    /// Nothing left to remember: no schedule running.
    pub fn is_idle(&self) -> bool {
        self.rings == 0
    }
}

/// The policy. Call [`Bell::observe`] first.
pub fn decide(bell: &Bell, facts: &Facts, config: &DoorbellConfig, now: Instant) -> Action {
    if !facts.idle || facts.held {
        return Action::Nothing;
    }
    let due = bell
        .last_ring
        .is_none_or(|last| now >= last + config.delay_after(bell.rings));
    match facts.inbox.queued.first() {
        Some(id) if due && ring_still_admitted(&facts.inbox, config, now) => {
            Action::Ring { id: id.clone() }
        }
        _ => Action::Nothing,
    }
}

/// Whether a native component leaves the doorbell to ring for this agent at
/// `now`: none is armed, and none announced `Native` within the last
/// [`DoorbellConfig::arm_grace`]. [`decide`] asks it, and a write asks it
/// again under the writer before each step of a ring, with the ring's own
/// message still queued, so a component that arms or announces, or a fetch
/// of that message, while the ring waits for the writer stops it.
pub fn ring_still_admitted(inbox: &DoorbellFacts, config: &DoorbellConfig, now: Instant) -> bool {
    // PRD #1666 §11: an incompatible integration gets no doorbell; its mail
    // waits queued under the card's notice.
    !inbox.blocked
        && inbox.armed == 0
        && inbox
            .native_announced_at
            .is_none_or(|at| now >= at + config.arm_grace)
}

/// PRD #1666 hardening H3: what a ring re-asks before admission and while
/// its transaction holds the pane's writer. The same admission
/// (`admission`, decided by the tick: nothing happened to the pane since)
/// and, for a ring, still no native component armed or inside the arm grace
/// of an announcement. Part 2 (`inbox/doorbell/031`): and the message the
/// ring names (`selected`) still queued for this generation, from a fresh
/// query each time.
///
/// Part 3, round 5-wake (plan §2.3): the check never waits on the hub's lock.
/// The ring holds the writer, and the hub's holders can themselves wait on
/// other locks, so a blocking read could hold the writer past the
/// transaction's deadline, which bounds only what the check awaits. The hub
/// is read with [`crate::inbox_hub::InboxHub::try_doorbell_facts`]; while it is
/// contended the check sleeps and asks again, the app state included, until
/// it answers or the ring's own deadline gives up on it. No guard is held
/// across an await.
pub(crate) fn ring_admission_check(
    state: SharedState,
    registry: Arc<AgentPtyRegistry>,
    pane: &str,
    agent: &str,
    config: &DoorbellConfig,
    selected: &str,
    admission: Option<crate::state::DoorbellAdmission>,
) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> + Send + Sync + use<>
{
    let (pane, agent, selected) = (pane.to_string(), agent.to_string(), selected.to_string());
    let config = config.clone();
    move || {
        let state = Arc::clone(&state);
        let registry = Arc::clone(&registry);
        let (pane, agent, selected) = (pane.clone(), agent.clone(), selected.clone());
        let config = config.clone();
        Box::pin(async move {
            loop {
                let now = state.read().await.doorbell_admission(&agent, &pane);
                if !admission
                    .zip(now)
                    .is_some_and(|(decided, now)| decided.still_holds(&now))
                {
                    return false;
                }
                if let Some(facts) = registry.inbox().try_doorbell_facts(&pane, &agent) {
                    return facts.queued.contains(&selected)
                        && ring_still_admitted(&facts, &config, Instant::now());
                }
                tokio::time::sleep(crate::doorbell_txn::SEAM_RETRY).await;
            }
        })
    }
}

/// PR #1735 fifth review (R5): whether an agent generation's waiting
/// messages have waited past [`DoorbellConfig::wake_stall_after`] with
/// nothing waking the agent for them.
///
/// The clock runs while messages are queued, no native component is armed
/// and the agent is not working — idle, in an error, or with no status at all
/// (a start dialog it never got past, an integration that never reported).
/// It starts again whenever the oldest queued message changes (something took
/// it), and stands still while the agent works, answers a question or is
/// blocked, which is waiting the user can already see. It is decided from
/// what the inbox and the agent report, not from why a ring was held, so it
/// covers every wake path: a doorbell that cannot ring (an unrecognised
/// composer, a draft left in the box), one rung and never answered, and a
/// native or turn-end hand-over that never came.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StallClock {
    oldest: Option<String>,
    since: Option<Instant>,
    raised: bool,
}

/// What [`StallClock::observe`] asks the inbox to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallChange {
    Raise,
    Clear,
    Nothing,
}

impl StallClock {
    /// One tick: `oldest` is the oldest message queued for the agent,
    /// `waiting` whether nothing is working on the agent's side (no armed
    /// component, the agent not working).
    pub fn observe(
        &mut self,
        oldest: Option<&str>,
        waiting: bool,
        now: Instant,
        after: Duration,
    ) -> StallChange {
        let Some(oldest) = oldest else {
            let raised = self.raised;
            *self = Self::default();
            return if raised {
                StallChange::Clear
            } else {
                StallChange::Nothing
            };
        };
        let mut change = StallChange::Nothing;
        if self.oldest.as_deref() != Some(oldest) {
            self.oldest = Some(oldest.to_string());
            self.since = None;
            if std::mem::take(&mut self.raised) {
                change = StallChange::Clear;
            }
        }
        if !waiting {
            self.since = None;
            if std::mem::take(&mut self.raised) {
                change = StallChange::Clear;
            }
            return change;
        }
        let since = *self.since.get_or_insert(now);
        if !self.raised && now >= since + after {
            self.raised = true;
            return StallChange::Raise;
        }
        change
    }
}

/// PRD #1666 part 3, round 5-wake (plan §5.1): what the doorbell's hold reason
/// for one agent generation is decided from on a tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HoldFacts {
    /// Messages are queued for the generation.
    pub queued: bool,
    /// A native component is armed or inside its arm grace, or an
    /// integration notice owns the card ([`ring_still_admitted`] says no).
    pub receiver: bool,
    /// The agent reports a status other than Idle and waiting for input:
    /// working, compacting, blocked, an error, or none yet.
    pub working: bool,
    /// The generation's first ring waits out its settle window (plan §4.4).
    pub settling: bool,
    /// The agent reports it waits for input (a dialog, a permission prompt).
    pub waiting_for_input: bool,
    /// A paste into the pane has not finished.
    pub paste_open: bool,
    /// The user may have an unsent draft in the pane.
    pub draft: bool,
    /// The deck may not type into the generation: its activation, its
    /// input-mode evidence, its spent attempt numbers or a missing submit
    /// route says no, with this clause.
    pub cannot_type: Option<&'static str>,
    /// The doorbell's last try found no screen it could type into (its
    /// composer not shown empty and ready, a dialog, a layout edge), or the
    /// pane's screen is not Clean.
    pub unrecognised: bool,
    /// The agent did not take the deck's last Enter in this episode.
    pub not_taken: bool,
    /// How many of the agent's turns have ended in the pane.
    pub turns: u64,
}

/// What a hold does to its episode's clock ([`HoldEpisode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldClock {
    /// Nothing is queued: the episode ends.
    Reset,
    /// Something the user can already see explains the wait.
    Paused,
    Runs,
}

/// PRD #1666 part 3, round 5-wake (plan §5.1): the obstacle in front of the
/// doorbell, as the reason a card would show and what it does to the
/// episode's clock. The first row that holds wins: nothing queued; a native
/// component armed or due, or an integration notice on the card; the agent
/// working; the first ring settling; the agent waiting for input
/// (`awaiting_prompt`); an open paste (`paste_open`); an unsent draft
/// (`possible_draft`); a generation the deck may not type into
/// (`cannot_type`); the last try finding no screen to type into, or a screen
/// that is not Clean (`awaiting_prompt`); an Enter the agent did not take
/// (`not_taken`); otherwise no reason, the clock running. A suspended
/// doorbell's reason (`uncertain_input`, `dialog_during_ring`) is the
/// publisher's, from the generation's latch cell, and shows over this one.
pub fn obstacle(facts: &HoldFacts) -> (Option<crate::inbox_display::WakeHold>, HoldClock) {
    use crate::inbox_display::WakeHold;
    if !facts.queued {
        (None, HoldClock::Reset)
    } else if facts.receiver || facts.working || facts.settling {
        (None, HoldClock::Paused)
    } else if facts.waiting_for_input {
        (Some(WakeHold::AwaitingPrompt), HoldClock::Runs)
    } else if facts.paste_open {
        (Some(WakeHold::PasteOpen), HoldClock::Runs)
    } else if facts.draft {
        (Some(WakeHold::PossibleDraft), HoldClock::Runs)
    } else if facts.cannot_type.is_some() {
        (Some(WakeHold::CannotType), HoldClock::Runs)
    } else if facts.unrecognised {
        (Some(WakeHold::AwaitingPrompt), HoldClock::Runs)
    } else if facts.not_taken {
        (Some(WakeHold::NotTaken), HoldClock::Runs)
    } else {
        (None, HoldClock::Runs)
    }
}

/// PRD #1666 part 3, round 5-wake (plan §5.1): one hold episode of an agent
/// generation, and the reason it last published. An episode ends when the
/// queue empties or one of the agent's turns ends; a new generation has its
/// own. Its clock counts only the time the obstacle lets it run, so busy time
/// is excluded, and a reason is published once that passes the stall window;
/// a reason that changes inside the episode replaces the published one
/// without restarting the clock, and taking one of several queued messages
/// leaves the episode running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HoldEpisode {
    turns: Option<u64>,
    ran: Duration,
    running_since: Option<Instant>,
    published: Option<crate::inbox_display::WakeHold>,
}

impl HoldEpisode {
    /// One tick. Returns the reason to publish when it differs from the one
    /// published last (`Some(None)` clears it).
    pub fn observe(
        &mut self,
        facts: &HoldFacts,
        now: Instant,
        after: Duration,
    ) -> Option<Option<crate::inbox_display::WakeHold>> {
        let (reason, clock) = obstacle(facts);
        if clock == HoldClock::Reset || self.turns.is_some_and(|turns| turns != facts.turns) {
            self.ran = Duration::ZERO;
            self.running_since = None;
        }
        self.turns = Some(facts.turns);
        if clock == HoldClock::Runs {
            self.running_since.get_or_insert(now);
        } else if let Some(since) = self.running_since.take() {
            self.ran += now.saturating_duration_since(since);
        }
        let elapsed = self.ran
            + self
                .running_since
                .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let shown = if elapsed >= after { reason } else { None };
        if shown == self.published {
            return None;
        }
        self.published = shown;
        Some(shown)
    }
}

/// PRD #1666 part 3, round 5-wake: whether the doorbell's last try found no
/// screen it could type into, after a ring that ended in `outcome`: a try
/// whose admission frame showed no empty ready composer, a dialog or a layout
/// edge holds as `awaiting_prompt` until a ring writes, and one that wrote
/// nothing for another reason leaves it as it was (`before`).
fn unrecognised_after(outcome: RingOutcome, before: bool) -> bool {
    use crate::doorbell_txn::{NotTyped, Outcome};
    match outcome {
        RingOutcome::Txn(Outcome::NothingWritten(
            NotTyped::AwaitingPrompt | NotTyped::Dialog(_) | NotTyped::Layout,
        )) => true,
        RingOutcome::Txn(
            Outcome::Submitted | Outcome::Withdrawn | Outcome::Unresolved(_) | Outcome::Retired,
        ) => false,
        RingOutcome::Txn(Outcome::NothingWritten(_))
        | RingOutcome::NotOwner
        | RingOutcome::NotReading
        | RingOutcome::Suspended => before,
    }
}

/// What a ring's report does to its generation's bell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RingEffect {
    /// It wrote into the pane: the schedule moves on, and the ring counts.
    pub(crate) rang: bool,
    /// The agent did not take its Enter: no more rings until a turn ends.
    pub(crate) not_taken: bool,
}

/// What a ring that ended in `outcome` does (plan §5.1): a transaction that
/// wrote counts as a ring, whatever it ended in; one that wrote nothing,
/// a token draw that found none free included, is no ring and leaves the
/// hold as it was; a withdrawal after an Enter reached the pane is an Enter
/// the agent did not take.
pub(crate) fn ring_effect(outcome: RingOutcome, cr_reached: bool) -> RingEffect {
    use crate::doorbell_txn::Outcome;
    match outcome {
        RingOutcome::Txn(Outcome::Submitted | Outcome::Unresolved(_)) => RingEffect {
            rang: true,
            not_taken: false,
        },
        RingOutcome::Txn(Outcome::Withdrawn) => RingEffect {
            rang: true,
            not_taken: cr_reached,
        },
        RingOutcome::Txn(Outcome::NothingWritten(_) | Outcome::Retired)
        | RingOutcome::NotOwner
        | RingOutcome::NotReading
        | RingOutcome::Suspended => RingEffect {
            rang: false,
            not_taken: false,
        },
    }
}

/// Whether an agent in `status` is doing something the user can already see,
/// so its messages waiting is not a stalled wake: working, compacting,
/// waiting on the user's answer, or blocked by a usage limit.
fn status_explains_waiting(status: Option<&crate::state::SessionStatus>) -> bool {
    use crate::state::SessionStatus;
    matches!(
        status,
        Some(
            SessionStatus::Thinking
                | SessionStatus::Working
                | SessionStatus::Compacting
                | SessionStatus::WaitingForInput
                | SessionStatus::Blocked
        )
    )
}

type AgentKey = (String, String);

/// E2E interleave fixture, configured below HOME by the isolated harness.
/// Directory checks assume a trusted fixture filesystem, including marker leaves.
/// The caller holds neither the app-state nor inbox-hub lock while parked.
#[cfg(feature = "e2e")]
pub(crate) async fn pause_rering_for_test(pane: &str, phase: &str) {
    // `*` pauses whichever pane gets there first, for a fixture that cannot
    // know its pane's id before the daemon starts.
    if !matches!(
        std::env::var("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE").as_deref(),
        Ok(paused) if paused == pane || paused == "*"
    ) {
        return;
    }
    let Some(root) = test_control_root() else {
        return;
    };
    if std::fs::remove_file(root.join(format!("{phase}.armed"))).is_err() {
        return;
    }
    std::fs::write(root.join(format!("{phase}.reached")), b"").expect("report ring barrier");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while !root.join(format!("{phase}.release")).exists() {
        if tokio::time::Instant::now() >= deadline {
            std::fs::write(root.join(format!("{phase}.timed-out")), b"barrier timeout")
                .expect("report barrier timeout");
            panic!("doorbell test barrier {phase} timed out after 8s for pane {pane}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(feature = "e2e")]
fn test_control_root() -> Option<std::path::PathBuf> {
    test_control_dir("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR")
}

/// The test-control directory the environment variable `var` names, one
/// component under HOME, created if missing. Also read by the work-done seams
/// (`crate::task_completion::e2e_seams`).
#[cfg(feature = "e2e")]
pub(crate) fn test_control_dir(var: &str) -> Option<std::path::PathBuf> {
    let root = std::env::var_os(var)?;
    let root = std::path::PathBuf::from(root);
    // Check the configured directory under HOME. The harness supplies a private
    // HOME and trusted marker leaves; this is not a no-follow filesystem boundary.
    if root.components().count() != 1
        || !matches!(
            root.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return None;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("fixture HOME"));
    let root = home.join(root);
    std::fs::create_dir_all(&root).ok()?;
    let canonical = root.canonicalize().ok()?;
    if !canonical.starts_with(home.canonicalize().expect("fixture HOME exists")) {
        return None;
    }
    Some(root)
}

/// Append-only observations, enabled by fixture configuration under HOME.
#[cfg(feature = "e2e")]
pub(crate) fn record_for_test(event: &str) {
    use std::io::Write;
    if let Some(root) = test_control_root() {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("observations"))
            .expect("open test observations");
        writeln!(file, "{event}").expect("record test observation");
    }
}

#[derive(Default)]
struct Slot {
    bell: Bell,
    /// A ring for this agent is in flight.
    busy: bool,
    /// PR #1735 fifth review (R5).
    stall: StallClock,
    /// PRD #1666 part 3, round 5-wake.
    hold: HoldEpisode,
    /// The last ring found no screen it could type into, and none has written
    /// since.
    unrecognised: bool,
    /// The agent did not take the deck's Enter, at this turn count: no more
    /// rings until a turn ends or the queue empties (plan §5.1, Q9).
    not_taken: Option<u64>,
    /// The turn count this episode's evidence-refresh nudge was sent at.
    nudged: Option<u64>,
}

// Test-only cancellation cleanup: a seam timeout/panic must not strand a
// logical busy slot, even when it unwinds while holding the pane writer.
#[cfg(feature = "e2e")]
struct TestBusyGuard {
    slots: Arc<Mutex<HashMap<AgentKey, Slot>>>,
    key: Option<AgentKey>,
}

#[cfg(feature = "e2e")]
impl Drop for TestBusyGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        if let Some(slot) = self
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&key)
        {
            slot.busy = false;
        }
    }
}

/// Run the doorbell for as long as the returned task lives.
pub fn spawn(
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    config: DoorbellConfig,
) -> tokio::task::JoinHandle<()> {
    let slots: Arc<Mutex<HashMap<AgentKey, Slot>>> = Arc::default();
    tokio::spawn(async move {
        #[cfg(feature = "e2e")]
        let clock_origin = Instant::now();
        let mut tick = tokio::time::interval(config.tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            #[cfg(feature = "e2e")]
            let now = Instant::now();
            // Advance policy age without sleeping. With e2e enabled, a
            // configured daemon reads this trusted fixture file.
            #[cfg(feature = "e2e")]
            let clock_step = test_control_root().and_then(|root| {
                let ms = std::fs::read_to_string(root.join("clock-ms"))
                    .ok()?
                    .parse::<u64>()
                    .ok()?;
                Some((root, ms))
            });
            #[cfg(feature = "e2e")]
            let now = clock_step
                .as_ref()
                .map_or(now, |(_, ms)| clock_origin + Duration::from_millis(*ms));
            #[cfg(feature = "e2e")]
            run_tick(&registry, &state, &config, &slots, now).await;
            #[cfg(not(feature = "e2e"))]
            run_tick(&registry, &state, &config, &slots).await;
            #[cfg(feature = "e2e")]
            if let Some((root, ms)) = clock_step {
                // Stop scheduling this controlled step while its attempts are
                // in flight. In particular, a writer-only readiness guard may
                // withhold every attempt; let that child finish before acking,
                // including on a one-worker runtime.
                while slots
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .any(|slot| slot.busy)
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                std::fs::write(root.join("clock-observed"), ms.to_string())
                    .expect("report completed policy tick");
            }
        }
    })
}

async fn run_tick(
    registry: &Arc<AgentPtyRegistry>,
    state: &SharedState,
    config: &DoorbellConfig,
    slots: &Arc<Mutex<HashMap<AgentKey, Slot>>>,
    #[cfg(feature = "e2e")] now: Instant,
) {
    let hub = registry.inbox();
    // Every agent generation with queued messages, plus every one the doorbell
    // still remembers. Only the pane's LIVE agent counts: a predecessor's
    // messages never ring in its replacement.
    let mut keys: Vec<AgentKey> = Vec::new();
    for pane in hub.panes_with_queued() {
        if let Some(agent) = registry.pane_current_agent_id(&pane) {
            keys.push((pane, agent));
        }
    }
    {
        let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.retain(|(pane, agent), slot| {
            slot.busy || registry.pane_current_agent_id(pane).as_deref() == Some(agent.as_str())
        });
        for key in slots.keys() {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
    }
    if keys.is_empty() {
        return;
    }
    // PRD #1666 S5: a pane whose session declared it cannot take input
    // (history-only or view-only) is never rung, whatever its status says —
    // the line would land in no live conversation. Part 2: and whether the
    // generation's own hooks have reported, which says whether its prompt
    // hook reports its prompts (the hook route of plan §1.4).
    type Admission = (
        Option<crate::state::DoorbellAdmission>,
        bool,
        Option<crate::state::SessionStatus>,
    );
    let admissions: Vec<Admission> = {
        let state = state.read().await;
        keys.iter()
            .map(|(pane, agent)| {
                let hooked = !matches!(
                    state.doorbell_readiness(agent, pane).0,
                    crate::state::ReadinessLevel::Unproven
                );
                (
                    state.doorbell_admission(agent, pane),
                    hooked,
                    state.agent_status(agent, pane),
                )
            })
            .collect()
    };
    #[cfg(not(feature = "e2e"))]
    let now = Instant::now();
    for ((pane, agent), (admission, hooked, status)) in keys.into_iter().zip(admissions) {
        let explained = status_explains_waiting(status.as_ref());
        let input =
            registry.pane_input_hold(&pane, registry.doorbell_input_semantics(&pane, &agent));
        let inbox = hub.doorbell_facts(&pane, &agent);
        let receiver = !ring_still_admitted(&inbox, config, now);
        let queued = !inbox.queued.is_empty();
        // Only asked while mail waits and nothing else takes it: each reads
        // the generation's activation and evidence.
        let refused = (queued && !receiver)
            .then(|| registry.doorbell_refusal(&pane, &agent, hooked))
            .flatten();
        // Q28: an agent whose first own hook comes with its first prompt may
        // be rung on its own measured proof before it reports.
        let ready = hooked
            || (queued
                && !receiver
                && refused.is_none()
                && registry.doorbell_ready_before_hooks(&pane, &agent));
        let settling = queued
            && !receiver
            && refused.is_none()
            && registry.doorbell_settling(&pane, &agent, hooked);
        let suspended = registry.doorbell_suspended(&agent);
        let dirty = queued && refused.is_none() && registry.doorbell_evidence_dirty(&agent);
        // PRD #1666 part 3 (Q35-18b, O1): the nudge waits until the agent's
        // own redraw is due by its generation's query-wait estimate. That
        // estimate is real time, so it is read against the real clock even
        // where an e2e build's policy clock runs ahead.
        let due = dirty && registry.doorbell_nudge_due(&agent, Instant::now());
        let (action, nudge, rering) = {
            let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slots.entry((pane.clone(), agent.clone())).or_default();
            let waiting = inbox.armed == 0 && !explained;
            match slot.stall.observe(
                inbox.queued.first().map(String::as_str),
                waiting,
                now,
                config.wake_stall_after,
            ) {
                StallChange::Raise => hub.set_wake_stalled(&pane, &agent, true),
                StallChange::Clear => hub.set_wake_stalled(&pane, &agent, false),
                StallChange::Nothing => {}
            }
            // An episode ends when the queue empties or a turn ends: a
            // not-taken Enter and the refresh nudge belong to one.
            if !queued || slot.not_taken.is_some_and(|turns| turns != input.turns) {
                slot.not_taken = None;
            }
            if !queued || slot.nudged.is_some_and(|turns| turns != input.turns) {
                slot.nudged = None;
            }
            let working = !matches!(
                status,
                Some(
                    crate::state::SessionStatus::Idle
                        | crate::state::SessionStatus::WaitingForInput
                )
            );
            let hold = HoldFacts {
                queued,
                receiver,
                working,
                settling,
                waiting_for_input: status == Some(crate::state::SessionStatus::WaitingForInput),
                paste_open: input.paste_open,
                draft: input.draft,
                cannot_type: refused,
                unrecognised: slot.unrecognised || dirty,
                not_taken: slot.not_taken.is_some(),
                turns: input.turns,
            };
            if let Some(shown) = slot.hold.observe(&hold, now, config.wake_stall_after) {
                info!(pane_id = %pane, agent_id = %agent, hold = ?shown, ?hold, "doorbell hold");
                hub.set_wake_hold(&pane, &agent, shown);
            }
            if slot.busy {
                continue;
            }
            let facts = Facts {
                inbox,
                // `inbox/doorbell/008`: a generation whose own hooks have not
                // reported is never rung on what its card or a predecessor's
                // Idle presents, only, for an agent that reports nothing
                // before its first prompt, on its own measured proof (Q28).
                idle: admission.is_some() && ready,
                held: input.paste_open
                    || refused.is_some()
                    || settling
                    || suspended
                    || slot.not_taken.is_some(),
            };
            slot.bell.observe(&facts);
            // Plan §2.2's evidence-refresh nudge: once per episode, between
            // attempts, for a screen that stays Dirty with nothing redrawing
            // it, once the agent's own redraw was due (O1).
            let nudge = due && facts.idle && !facts.held && !working && slot.nudged.is_none();
            if nudge {
                slot.nudged = Some(input.turns);
            }
            let action = decide(&slot.bell, &facts, config, now);
            if action == Action::Nothing {
                if slot.bell.is_idle() && facts.inbox.queued.is_empty() {
                    slots.remove(&(pane.clone(), agent.clone()));
                }
                (None, nudge, false)
            } else {
                slot.busy = true;
                (
                    Some((action, facts.inbox.queued.clone())),
                    nudge,
                    slot.bell.rings > 0,
                )
            }
        };
        if nudge {
            debug!(pane_id = %pane, agent_id = %agent, "doorbell evidence-refresh nudge");
            registry.nudge_repaint(&agent);
        }
        let Some((Action::Ring { id }, covered)) = action else {
            continue;
        };
        #[cfg(not(feature = "e2e"))]
        let _ = rering;
        let registry = Arc::clone(registry);
        let slots = Arc::clone(slots);
        let state = Arc::clone(state);
        let config = config.clone();
        tokio::spawn(async move {
            #[cfg(feature = "e2e")]
            let mut _busy_guard = TestBusyGuard {
                slots: Arc::clone(&slots),
                key: Some((pane.clone(), agent.clone())),
            };
            #[cfg(feature = "e2e")]
            if rering {
                pause_rering_for_test(&pane, "decided").await;
            }
            let still_admitted = ring_admission_check(
                Arc::clone(&state),
                Arc::clone(&registry),
                &pane,
                &agent,
                &config,
                &id,
                admission,
            );
            let Some(message) = crate::inbox::MessageId::parse(&id) else {
                warn!(pane_id = %pane, agent_id = %agent, message_id = %id, "doorbell: a queued id that does not parse");
                return;
            };
            let report = registry
                .ring_doorbell(&pane, &agent, message, hooked, still_admitted)
                .await;
            // The writer has been released; busy remains set so a completed
            // attempt can be inspected without a later retry changing bytes.
            #[cfg(feature = "e2e")]
            pause_rering_for_test(&pane, "completed").await;
            let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg(feature = "e2e")]
            {
                // Normal completion owns the slot lock: disarm before making
                // it available, so Drop cannot clear a newer attempt's busy bit.
                _busy_guard.key = None;
            }
            let Some(slot) = slots.get_mut(&(pane.clone(), agent.clone())) else {
                return;
            };
            slot.busy = false;
            slot.unrecognised = unrecognised_after(report.outcome, slot.unrecognised);
            let effect = ring_effect(report.outcome, report.cr_reached);
            if effect.rang {
                slot.bell.rang(Instant::now(), covered);
                registry.inbox().note_ring(&pane, &agent);
            }
            if effect.not_taken {
                // Q9: the agent did not take the deck's Enter. No more rings
                // until its next turn.
                slot.not_taken = Some(registry.pane_input_hold(&pane, None).turns);
            }
            use crate::doorbell_txn::Outcome;
            match report.outcome {
                RingOutcome::Txn(Outcome::Submitted) => {
                    info!(
                        pane_id = %pane,
                        agent_id = %agent,
                        message_id = %id,
                        attempt = report.attempt,
                        rings = slot.bell.rings,
                        "doorbell rang"
                    );
                }
                RingOutcome::Txn(Outcome::Withdrawn) => {
                    info!(
                        pane_id = %pane,
                        agent_id = %agent,
                        message_id = %id,
                        attempt = report.attempt,
                        not_taken = report.cr_reached,
                        "doorbell ring withdrawn"
                    );
                }
                RingOutcome::Txn(Outcome::Unresolved(cause)) => {
                    warn!(
                        pane_id = %pane,
                        agent_id = %agent,
                        message_id = %id,
                        attempt = report.attempt,
                        ?cause,
                        "doorbell ring unresolved; the doorbell is suspended until the user submits or clears"
                    );
                }
                outcome => {
                    debug!(pane_id = %pane, agent_id = %agent, ?outcome, "doorbell ring not admitted; nothing typed");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// PR #1735 fifth review (R5): the stall clock raises once, after the
    /// oldest queued message has waited past the window with nothing working
    /// on the agent's side; stands still while the agent works; starts again
    /// when something takes the oldest message; and clears when the queue
    /// empties or the agent starts working.
    #[test]
    fn a_wake_stalls_only_past_its_window_for_an_agent_not_working() {
        let after = Duration::from_secs(120);
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut clock = StallClock::default();
        assert_eq!(
            clock.observe(Some("m1"), true, at(0), after),
            StallChange::Nothing
        );
        assert_eq!(
            clock.observe(Some("m1"), true, at(119), after),
            StallChange::Nothing
        );
        assert_eq!(
            clock.observe(Some("m1"), true, at(120), after),
            StallChange::Raise
        );
        assert_eq!(
            clock.observe(Some("m1"), true, at(500), after),
            StallChange::Nothing,
            "raised once"
        );
        assert_eq!(
            clock.observe(Some("m1"), false, at(501), after),
            StallChange::Clear,
            "the agent started working"
        );
        assert_eq!(
            clock.observe(Some("m1"), true, at(502), after),
            StallChange::Nothing
        );
        assert_eq!(
            clock.observe(Some("m1"), true, at(621), after),
            StallChange::Nothing,
            "the window starts again once the agent stops working"
        );
        assert_eq!(
            clock.observe(Some("m1"), true, at(622), after),
            StallChange::Raise
        );
        assert_eq!(
            clock.observe(Some("m2"), true, at(623), after),
            StallChange::Clear,
            "something took the oldest message"
        );
        assert_eq!(
            clock.observe(Some("m2"), true, at(743), after),
            StallChange::Raise
        );
        assert_eq!(
            clock.observe(None, true, at(744), after),
            StallChange::Clear
        );
        assert_eq!(
            clock.observe(None, true, at(745), after),
            StallChange::Nothing
        );
        let mut working = StallClock::default();
        for s in [0, 200, 400] {
            assert_eq!(
                working.observe(Some("m1"), false, at(s), after),
                StallChange::Nothing,
                "an agent at work is never shown as waiting to wake"
            );
        }
    }

    fn held(queued: bool) -> HoldFacts {
        HoldFacts {
            queued,
            ..HoldFacts::default()
        }
    }

    /// Scenario: Walk one agent's hold episode through the doorbell's obstacle table with a 120 s window: a message waits while the agent reports it waits for input, before any ring was tried; the agent then works a while, its draft and an open paste come and go, a second message is taken, a turn ends and the queue empties. The reason appears only once the not-working time passes the window, the working time does not count, a changed reason replaces the shown one without starting the count again, an ended turn and an empty queue start it over, and an armed component or a working agent shows none.
    #[spec("inbox/doorbell/041")]
    #[test]
    fn doorbell_041_a_hold_reason_shows_once_its_episode_passes_the_window() {
        use crate::inbox_display::WakeHold;
        let after = Duration::from_secs(120);
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut episode = HoldEpisode::default();
        let waiting = HoldFacts {
            waiting_for_input: true,
            ..held(true)
        };
        // First-ever WaitingForInput, before any ring was tried.
        assert_eq!(episode.observe(&waiting, at(0), after), None);
        assert_eq!(episode.observe(&waiting, at(119), after), None);
        assert_eq!(
            episode.observe(&waiting, at(120), after),
            Some(Some(WakeHold::AwaitingPrompt))
        );
        assert_eq!(
            episode.observe(&waiting, at(121), after),
            None,
            "shown once"
        );
        // Working hides it and stops the clock; idle again with a draft shows
        // the draft at once, the episode's clock already past the window.
        let working = HoldFacts {
            working: true,
            ..held(true)
        };
        assert_eq!(episode.observe(&working, at(130), after), Some(None));
        assert_eq!(episode.observe(&working, at(900), after), None);
        let draft = HoldFacts {
            draft: true,
            ..held(true)
        };
        assert_eq!(
            episode.observe(&draft, at(901), after),
            Some(Some(WakeHold::PossibleDraft))
        );
        // A paste open as well wins over the draft; closing it goes back.
        let paste = HoldFacts {
            paste_open: true,
            ..draft
        };
        assert_eq!(
            episode.observe(&paste, at(902), after),
            Some(Some(WakeHold::PasteOpen))
        );
        assert_eq!(
            episode.observe(&draft, at(903), after),
            Some(Some(WakeHold::PossibleDraft)),
            "a reason flapping inside the episode never restarts its clock"
        );
        // Taking one of two queued messages leaves the episode as it is:
        // `queued` stays true, and nothing is said again.
        assert_eq!(episode.observe(&draft, at(904), after), None);
        // A component arming shows nothing.
        let armed = HoldFacts {
            receiver: true,
            ..draft
        };
        assert_eq!(episode.observe(&armed, at(905), after), Some(None));
        assert_eq!(
            episode.observe(&draft, at(906), after),
            Some(Some(WakeHold::PossibleDraft))
        );
        // An ended turn starts a new episode: nothing until the window passes
        // again.
        let next_turn = HoldFacts { turns: 1, ..draft };
        assert_eq!(episode.observe(&next_turn, at(907), after), Some(None));
        assert_eq!(episode.observe(&next_turn, at(1026), after), None);
        assert_eq!(
            episode.observe(&next_turn, at(1027), after),
            Some(Some(WakeHold::PossibleDraft))
        );
        // The queue empties: the episode ends, and mail arriving later starts
        // a new one from zero.
        let empty = HoldFacts {
            queued: false,
            ..next_turn
        };
        assert_eq!(episode.observe(&empty, at(1028), after), Some(None));
        assert_eq!(episode.observe(&next_turn, at(1029), after), None);
        assert_eq!(episode.observe(&next_turn, at(1148), after), None);
        assert_eq!(
            episode.observe(&next_turn, at(1149), after),
            Some(Some(WakeHold::PossibleDraft))
        );
    }

    /// Scenario: Check the obstacle table row by row: each fact that holds a ring maps to its reason in priority order (waiting for input, an open paste, a draft, a generation the deck may not type into, no screen to type into, an Enter not taken), an armed component, a working agent and a settling first ring show none with the clock stopped, nothing queued ends the episode, and a pane with nothing in the way runs the clock with no reason. Then check that a try whose admission found no empty ready composer, a dialog or a layout edge holds as awaiting a prompt until a ring writes, and that a try that wrote nothing for another reason leaves that as it was.
    #[spec("inbox/doorbell/041")]
    #[test]
    fn doorbell_041_the_obstacle_table_takes_the_first_row_that_holds() {
        use crate::doorbell_txn::{Cause, NotTyped, Outcome};
        use crate::inbox_display::WakeHold;
        let all = HoldFacts {
            queued: true,
            receiver: true,
            working: true,
            settling: true,
            waiting_for_input: true,
            paste_open: true,
            draft: true,
            cannot_type: Some("unmeasured_release"),
            unrecognised: true,
            not_taken: true,
            turns: 0,
        };
        assert_eq!(
            obstacle(&HoldFacts {
                queued: false,
                ..all
            }),
            (None, HoldClock::Reset)
        );
        assert_eq!(obstacle(&all), (None, HoldClock::Paused));
        let all = HoldFacts {
            receiver: false,
            ..all
        };
        assert_eq!(obstacle(&all), (None, HoldClock::Paused));
        let all = HoldFacts {
            working: false,
            ..all
        };
        assert_eq!(
            obstacle(&all),
            (None, HoldClock::Paused),
            "a settling first ring pauses the clock"
        );
        let all = HoldFacts {
            settling: false,
            ..all
        };
        assert_eq!(
            obstacle(&all),
            (Some(WakeHold::AwaitingPrompt), HoldClock::Runs)
        );
        let all = HoldFacts {
            waiting_for_input: false,
            ..all
        };
        assert_eq!(obstacle(&all), (Some(WakeHold::PasteOpen), HoldClock::Runs));
        let all = HoldFacts {
            paste_open: false,
            ..all
        };
        assert_eq!(
            obstacle(&all),
            (Some(WakeHold::PossibleDraft), HoldClock::Runs)
        );
        let all = HoldFacts {
            draft: false,
            ..all
        };
        assert_eq!(
            obstacle(&all),
            (Some(WakeHold::CannotType), HoldClock::Runs)
        );
        let all = HoldFacts {
            cannot_type: None,
            ..all
        };
        assert_eq!(
            obstacle(&all),
            (Some(WakeHold::AwaitingPrompt), HoldClock::Runs)
        );
        let all = HoldFacts {
            unrecognised: false,
            ..all
        };
        assert_eq!(obstacle(&all), (Some(WakeHold::NotTaken), HoldClock::Runs));
        let all = HoldFacts {
            not_taken: false,
            ..all
        };
        assert_eq!(obstacle(&all), (None, HoldClock::Runs));

        for found_none in [
            NotTyped::AwaitingPrompt,
            NotTyped::Dialog("trust"),
            NotTyped::Layout,
        ] {
            assert!(
                unrecognised_after(RingOutcome::Txn(Outcome::NothingWritten(found_none)), false),
                "{found_none:?}"
            );
        }
        for wrote in [
            Outcome::Submitted,
            Outcome::Withdrawn,
            Outcome::Unresolved(Cause::NoSubmitEvidence),
            Outcome::Retired,
        ] {
            assert!(
                !unrecognised_after(RingOutcome::Txn(wrote), true),
                "{wrote:?}"
            );
        }
        for nothing in [
            RingOutcome::Txn(Outcome::NothingWritten(NotTyped::HumanInput)),
            RingOutcome::Txn(Outcome::NothingWritten(NotTyped::Settling)),
            RingOutcome::NotOwner,
            RingOutcome::NotReading,
            RingOutcome::Suspended,
        ] {
            assert!(unrecognised_after(nothing, true), "{nothing:?}");
            assert!(!unrecognised_after(nothing, false), "{nothing:?}");
        }
    }

    /// Scenario: Map each way a ring can end onto what it does to the bell: a submitted, unresolved or withdrawn transaction counts as a ring, a withdrawal after an Enter reached the pane is also an Enter not taken, and a ring that wrote nothing (a token draw that found none free, a human operation, a suspended doorbell, a pane gone or not reading) is no ring and leaves the hold as it was.
    #[spec("inbox/doorbell/041")]
    #[test]
    fn doorbell_041_only_a_ring_that_wrote_counts_and_no_token_is_none() {
        use crate::doorbell_txn::{Cause, NotTyped, Outcome};
        let rang = RingEffect {
            rang: true,
            not_taken: false,
        };
        let none = RingEffect {
            rang: false,
            not_taken: false,
        };
        assert_eq!(
            ring_effect(RingOutcome::Txn(Outcome::Submitted), true),
            rang
        );
        assert_eq!(
            ring_effect(
                RingOutcome::Txn(Outcome::Unresolved(Cause::NoSubmitEvidence)),
                true
            ),
            rang
        );
        assert_eq!(
            ring_effect(RingOutcome::Txn(Outcome::Withdrawn), false),
            rang
        );
        assert_eq!(
            ring_effect(RingOutcome::Txn(Outcome::Withdrawn), true),
            RingEffect {
                rang: true,
                not_taken: true,
            }
        );
        for nothing in [
            RingOutcome::Txn(Outcome::NothingWritten(NotTyped::NoToken)),
            RingOutcome::Txn(Outcome::NothingWritten(NotTyped::HumanInput)),
            RingOutcome::Txn(Outcome::Retired),
            RingOutcome::Suspended,
            RingOutcome::NotOwner,
            RingOutcome::NotReading,
        ] {
            assert_eq!(ring_effect(nothing, true), none, "{nothing:?}");
        }
        // No token is no screen fact either: the hold carries on as it was.
        let no_token = RingOutcome::Txn(Outcome::NothingWritten(NotTyped::NoToken));
        assert!(unrecognised_after(no_token, true));
        assert!(!unrecognised_after(no_token, false));
    }

    /// A working, compacting, asking or blocked agent explains its waiting
    /// messages; an idle one, one in an error and one with no status do not.
    #[test]
    fn only_a_visible_occupation_explains_waiting_messages() {
        use crate::state::SessionStatus;
        for status in [
            SessionStatus::Thinking,
            SessionStatus::Working,
            SessionStatus::Compacting,
            SessionStatus::WaitingForInput,
            SessionStatus::Blocked,
        ] {
            assert!(status_explains_waiting(Some(&status)), "{status:?}");
        }
        for status in [Some(SessionStatus::Idle), Some(SessionStatus::Error), None] {
            assert!(!status_explains_waiting(status.as_ref()), "{status:?}");
        }
    }

    #[test]
    fn the_wake_stall_window_reads_from_the_environment() {
        let config = DoorbellConfig::from_lookup(|name| {
            (name == ENV_WAKE_STALL_AFTER_MS).then(|| "1500".to_string())
        });
        assert_eq!(config.wake_stall_after, Duration::from_millis(1500));
        let zero = DoorbellConfig::from_lookup(|name| {
            (name == ENV_WAKE_STALL_AFTER_MS).then(|| "0".to_string())
        });
        assert_eq!(zero.wake_stall_after, DEFAULT_WAKE_STALL_AFTER);
    }

    fn facts(queued: &[&str]) -> Facts {
        Facts {
            inbox: DoorbellFacts {
                queued: queued.iter().map(|s| s.to_string()).collect(),
                armed: 0,
                native_announced_at: None,
                blocked: false,
            },
            idle: true,
            held: false,
        }
    }

    fn ring(id: &str) -> Action {
        Action::Ring { id: id.to_string() }
    }

    fn step(bell: &mut Bell, facts: &Facts, config: &DoorbellConfig, now: Instant) -> Action {
        bell.observe(facts);
        let action = decide(bell, facts, config, now);
        if let Action::Ring { .. } = action {
            bell.rang(now, facts.inbox.queued.clone());
        }
        action
    }

    /// Scenario: Feed the policy an empty inbox, then one queued message under
    /// each guard in turn (not Idle, a generation the deck is holding, an
    /// armed waiter, a Native announcement inside its grace). It must ring
    /// only once every guard passes, and never with an empty queue.
    #[spec("inbox/doorbell/013")]
    #[test]
    fn doorbell_013_rings_only_when_every_guard_passes() {
        let config = DoorbellConfig::default();
        let now = Instant::now();
        let bell = Bell::default();
        assert_eq!(decide(&bell, &facts(&[]), &config, now), Action::Nothing);
        let mut busy = facts(&["m1"]);
        busy.idle = false;
        assert_eq!(decide(&bell, &busy, &config, now), Action::Nothing);
        let mut held = facts(&["m1"]);
        held.held = true;
        assert_eq!(decide(&bell, &held, &config, now), Action::Nothing);
        let mut armed = facts(&["m1"]);
        armed.inbox.armed = 1;
        assert_eq!(decide(&bell, &armed, &config, now), Action::Nothing);
        let mut native = facts(&["m1"]);
        native.inbox.native_announced_at = Some(now);
        assert_eq!(decide(&bell, &native, &config, now), Action::Nothing);
        assert_eq!(
            decide(&bell, &native, &config, now + config.arm_grace),
            ring("m1")
        );
        assert_eq!(decide(&bell, &facts(&["m1"]), &config, now), ring("m1"));
    }

    /// Scenario: Leave two messages queued and step the policy through time.
    /// One ring covers both and names the oldest; it re-rings after each
    /// schedule delay, the last delay repeats, a fetch of the first rings for
    /// the second on the running schedule, and an emptied queue restarts the
    /// schedule for the next message, as does a queue holding none of the
    /// messages the last ring covered.
    #[spec("inbox/doorbell/014")]
    #[test]
    fn doorbell_014_coalesces_and_reschedules() {
        let config = DoorbellConfig {
            schedule: vec![Duration::from_secs(1), Duration::from_secs(2)],
            ..DoorbellConfig::default()
        };
        let t0 = Instant::now();
        let both = facts(&["m1", "m2"]);
        let mut bell = Bell::default();
        assert_eq!(step(&mut bell, &both, &config, t0), ring("m1"));
        let at = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(step(&mut bell, &both, &config, at(999)), Action::Nothing);
        assert_eq!(step(&mut bell, &both, &config, at(1000)), ring("m1"));
        assert_eq!(step(&mut bell, &both, &config, at(2999)), Action::Nothing);
        assert_eq!(step(&mut bell, &both, &config, at(3000)), ring("m1"));
        assert_eq!(step(&mut bell, &both, &config, at(5000)), ring("m1"));
        let rest = facts(&["m2"]);
        assert_eq!(step(&mut bell, &rest, &config, at(5001)), Action::Nothing);
        assert_eq!(step(&mut bell, &rest, &config, at(7000)), ring("m2"));
        let empty = facts(&[]);
        assert_eq!(step(&mut bell, &empty, &config, at(7001)), Action::Nothing);
        assert!(bell.is_idle());
        assert_eq!(
            step(&mut bell, &facts(&["m3"]), &config, at(9001)),
            ring("m3")
        );
        // The agent fetched m3 before the deck confirmed that ring, and m4
        // arrived before a tick saw the queue empty: a ring none of whose
        // messages is still queued starts no schedule for m4.
        assert_eq!(
            step(&mut bell, &facts(&["m4"]), &config, at(9002)),
            ring("m4")
        );
    }

    /// Scenario: Ring once, then let the message be fetched while the agent is idle, busy, holding a draft or with a waiter armed. Nothing follows a ring that went: the policy decides nothing for an emptied queue under any of them, and owes no CR of its own.
    #[spec("inbox/doorbell/015")]
    #[test]
    fn doorbell_015_nothing_follows_a_ring_once_its_message_is_taken() {
        let config = DoorbellConfig::default();
        let now = Instant::now();
        for (why, tweak) in [
            ("idle", (|_: &mut Facts| {}) as fn(&mut Facts)),
            ("busy", |f: &mut Facts| f.idle = false),
            ("held", |f: &mut Facts| f.held = true),
            ("armed", |f: &mut Facts| f.inbox.armed = 1),
        ] {
            let mut bell = Bell::default();
            assert_eq!(step(&mut bell, &facts(&["m1"]), &config, now), ring("m1"));
            let mut taken = facts(&[]);
            tweak(&mut taken);
            assert_eq!(
                step(&mut bell, &taken, &config, now + Duration::from_secs(600)),
                Action::Nothing,
                "{why}"
            );
            assert!(bell.is_idle(), "{why}");
        }
    }

    /// Scenario: Ring for a message, then keep it queued past every schedule delay while the generation is held (the deck may not type into it, it settles, its doorbell is suspended, or the agent did not take the last Enter). The policy rings for none of it while held, and once the hold ends the next due ring goes, naming the message.
    #[spec("inbox/doorbell/017")]
    #[test]
    fn doorbell_017_a_held_generation_is_not_rung_until_the_hold_ends() {
        let config = DoorbellConfig {
            schedule: vec![Duration::from_secs(1)],
            ..DoorbellConfig::default()
        };
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut bell = Bell::default();
        assert_eq!(step(&mut bell, &facts(&["m1"]), &config, t0), ring("m1"));
        let mut held = facts(&["m1"]);
        held.held = true;
        for due in [1000, 5000, 60_000] {
            assert_eq!(step(&mut bell, &held, &config, at(due)), Action::Nothing);
        }
        assert_eq!(
            step(&mut bell, &facts(&["m1"]), &config, at(60_001)),
            ring("m1")
        );
    }

    /// Scenario: Ring for two messages, then fetch the first just as a ring for the second is due, and keep the second queued over several delays. Each due ring names the second message alone, one ring per delay, and the queue emptying ends the rings.
    #[spec("inbox/doorbell/018")]
    #[test]
    fn doorbell_018_a_ring_for_remaining_work_names_it_alone() {
        let config = DoorbellConfig {
            schedule: vec![Duration::from_secs(1)],
            ..DoorbellConfig::default()
        };
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut bell = Bell::default();
        assert_eq!(
            step(&mut bell, &facts(&["m1", "m2"]), &config, t0),
            ring("m1")
        );
        let rest = facts(&["m2"]);
        for due in [1000, 2000, 3000] {
            assert_eq!(step(&mut bell, &rest, &config, at(due)), ring("m2"));
            assert_eq!(
                step(&mut bell, &rest, &config, at(due + 1)),
                Action::Nothing,
                "one ring per due delay"
            );
        }
        let empty = facts(&[]);
        assert_eq!(step(&mut bell, &empty, &config, at(3001)), Action::Nothing);
        assert_eq!(step(&mut bell, &empty, &config, at(9000)), Action::Nothing);
    }

    /// PR #1735 review: a native component that announces while a ring waits
    /// for the writer stops it, as an armed waiter does, until its arm grace
    /// has passed.
    #[test]
    fn a_ring_waiting_for_the_writer_yields_to_a_native_announcement() {
        let config = DoorbellConfig::default();
        let now = Instant::now();
        let mut inbox = facts(&["m1"]).inbox;
        assert!(ring_still_admitted(&inbox, &config, now));
        inbox.native_announced_at = Some(now);
        assert!(!ring_still_admitted(&inbox, &config, now));
        assert!(ring_still_admitted(&inbox, &config, now + config.arm_grace));
        inbox.armed = 1;
        assert!(!ring_still_admitted(
            &inbox,
            &config,
            now + config.arm_grace
        ));
    }

    /// Scenario: Parse the doorbell's environment overrides, including bad
    /// values. Valid values replace the defaults, a schedule keeps its order,
    /// and anything unparsable or zero leaves the default in place.
    #[spec("inbox/doorbell/016")]
    #[test]
    fn doorbell_016_env_overrides() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let config = DoorbellConfig::from_lookup(env(&[
            (ENV_TICK_MS, "25"),
            (ENV_SCHEDULE_MS, "300, 500,700"),
            (ENV_ARM_GRACE_MS, "0"),
        ]));
        assert_eq!(config.tick, Duration::from_millis(25));
        assert_eq!(config.delay_after(1), Duration::from_millis(300));
        assert_eq!(config.delay_after(3), Duration::from_millis(700));
        assert_eq!(config.delay_after(9), Duration::from_millis(700));
        assert_eq!(config.arm_grace, Duration::ZERO);
        let bad = DoorbellConfig::from_lookup(env(&[
            (ENV_TICK_MS, "0"),
            (ENV_SCHEDULE_MS, "300,,x"),
            (ENV_ARM_GRACE_MS, "soon"),
        ]));
        assert_eq!(bad, DoorbellConfig::default());
        assert_eq!(
            doorbell_line("m-k3xzq2ab-0f3c9a71d2e4b850"),
            " dot-agent-deck: message m-k3xzq2ab-0f3c9a71d2e4b850 waiting, run: dot-agent-deck inbox fetch"
        );
    }
}
