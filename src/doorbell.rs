//! PRD #1666 S4: the doorbell — the one line the deck still types into an
//! agent's pane.
//!
//! A message waits in the agent's inbox ([`crate::inbox_hub`]). When no
//! agent-side component is armed to take it, the deck types one fixed line that
//! names a waiting message and the command that fetches it:
//!
//! ```text
//!  dot-agent-deck: message m-k3xzq2ab-0f3c9a71d2e4b850 waiting, run: dot-agent-deck inbox fetch
//! ```
//!
//! The content itself is never typed. The leading space keeps two rings that
//! end up on one line readable.
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
//!    - part 2 (`inbox/doorbell/008`): that Idle is what the card presents;
//!      the generation must also be ready by its OWN reports, kept per
//!      generation outside the event journal
//!      ([`crate::state::AppState::doorbell_readiness`]). Its own Idle makes
//!      it ready; so does its own `SessionStart`, except for an agent that
//!      posts it before its prompt is up (Claude Code), which is then rung
//!      only while the writer recognises that prompt on screen. A
//!      placeholder card, the shell monitor's frames (which can carry a
//!      predecessor's session) and a wrapper's boot report establish none of
//!      it, and neither does how long the card or process has existed;
//!    - until the agent's own hooks have reported for this generation, an
//!      agent whose spec names a composer (Codex) is written to only while that
//!      composer is on screen: its wrapper reports Idle as soon as Codex takes
//!      raw input, which is while its start dialogs are still up, and any byte
//!      can answer one of those. Checked under the pane's writer, for the CRs
//!      too ([`crate::agent_pty::AgentPtyRegistry::write_doorbell`]);
//! 4. the user has no unsent draft in the pane — with no cap: the doorbell
//!    waits for the user however long they take;
//! 5. the ring is due on the schedule ([`DoorbellConfig::schedule`]).
//!
//! Conditions 2 to 4 are asked again under the pane's writer before each step
//! of a write (the pre-ring CR, the line, its submit CR), after the wait
//! between steps, and so is condition 1 for the **one message the ring
//! names**: if it was fetched or acknowledged since the decision, the rest of
//! the ring is withheld even while other mail stays queued (part 2,
//! `inbox/doorbell/031`). A human keystroke that arrives while the doorbell
//! holds the writer withholds the rest too. These are checks before each
//! write, not one transaction with the inbox: an acknowledgement that lands
//! between the last check and the PTY taking that step's bytes does not stop
//! that one step.
//!
//! It is **coalesced per agent**: one ring covers every queued message, and the
//! schedule restarts only once the queue has emptied.
//!
//! **Flushing.** A ring can be left in the composer unsubmitted (its CR landed
//! where the agent's TUI did not take it as Enter). So while a ring is
//! *outstanding* — rung, with none of the messages it covered fetched since —
//! the next ring is preceded by a bare CR, and once a covered message leaves
//! `queued` by any route, one bare CR is sent under the same guards. Both are
//! keyed on inbox state, never on the screen. For an agent whose composer is
//! gated (Codex), that CR is the one write the gate lets into a non-empty
//! composer, and only one showing the exact line this doorbell last typed
//! into the pane ([`Bell::unsubmitted`]), with the agent's cursor where an
//! Enter that was not taken leaves it; it then goes alone. A ring that only
//! flushed keeps that line recorded, so if the agent swallowed the CR too,
//! the next ring flushes again. The record lasts until the cleanup CR goes or
//! a ring types a new line, which can be after the message it names has left
//! `queued`: while other work is queued, a ring due for that work is sent in
//! place of the owed cleanup, and if it only flushes it keeps the record. So
//! while a frame matching the recorded line stays on screen and a message
//! stays `queued`, each due ring the guards admit sends one lone CR, with no
//! fixed total. That is deliberate: the flushed line runs `inbox fetch`,
//! which serves whatever is queued, and dropping the record would leave that
//! work behind a stranded line the deck sends no byte to. A flush that Codex
//! also takes as a newline leaves the line above two blank composer rows with
//! the cursor on the second, which the recogniser does not match, so the
//! flushing stops there; that holds for that rendered layout, not for every
//! way a renderer might draw it. The match is on what the screen shows, not
//! proof of where it came from: anything that can draw the pane can draw that
//! line and earn the CR, each time, and it goes to whatever then owns the
//! pane's input ([`crate::agent_registry::codex_composer_holds_doorbell_line`]).
//!
//! [`decide`] is the pure policy; [`spawn`] runs it against the daemon.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::agent_pty::{AgentPtyRegistry, DoorbellReady, DoorbellWrite};
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
    /// The last hook-reported status is Idle, and the generation is ready by
    /// its own reports or rung only through its composer gate.
    pub idle: bool,
    /// The user has an unsent draft in the pane.
    pub draft: bool,
}

/// The doorbell's memory of one agent generation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bell {
    /// Rings since the queue last emptied.
    pub rings: u32,
    /// When the last ring finished.
    pub last_ring: Option<Instant>,
    /// The messages the last ring covered, while that ring is outstanding;
    /// empty when it is not.
    pub outstanding: Vec<String>,
    /// A covered message left `queued` while its ring was outstanding, so one
    /// bare CR is owed.
    pub cleanup_owed: bool,
    /// The line the doorbell last typed, while a CR that may submit it is
    /// still owed (an outstanding ring or a cleanup). A ring that only
    /// flushed it keeps it, since that CR may have been swallowed too; the
    /// cleanup CR ([`Self::cleaned`]) and a ring that types a new line
    /// replace it. Its message leaving `queued` does not: a ring due for
    /// other queued work goes in place of the owed cleanup, and if it only
    /// flushes, the record outlives that message (`inbox/doorbell/018`). A
    /// gated composer showing anything else gets no byte
    /// ([`AgentPtyRegistry::write_doorbell`]).
    pub unsubmitted: Option<String>,
}

/// What the doorbell should do for one agent generation now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Type the line naming `id`, after a bare CR when `pre_cr`.
    Ring {
        id: String,
        pre_cr: bool,
    },
    /// Send one bare CR.
    Cleanup,
}

impl Bell {
    /// Fold this tick's inbox state in: an outstanding ring whose messages
    /// were fetched is owed its cleanup CR, and an empty queue restarts the
    /// schedule.
    pub fn observe(&mut self, facts: &Facts) {
        if !self.outstanding.is_empty()
            && self
                .outstanding
                .iter()
                .any(|id| !facts.inbox.queued.contains(id))
        {
            self.outstanding.clear();
            self.cleanup_owed = true;
        }
        if facts.inbox.queued.is_empty() {
            self.rings = 0;
            self.last_ring = None;
        }
    }

    /// Record a ring that covered `queued`. What it left in the box is the
    /// caller's to record in [`Self::unsubmitted`].
    pub fn rang(&mut self, now: Instant, queued: &[String]) {
        self.rings = self.rings.saturating_add(1);
        self.last_ring = Some(now);
        self.outstanding = queued.to_vec();
        self.cleanup_owed = false;
        self.unsubmitted = None;
    }

    /// Record the cleanup CR.
    pub fn cleaned(&mut self) {
        self.cleanup_owed = false;
        self.unsubmitted = None;
    }

    /// Nothing left to remember: no ring outstanding or owed and no schedule
    /// running.
    pub fn is_idle(&self) -> bool {
        self.rings == 0 && self.outstanding.is_empty() && !self.cleanup_owed
    }
}

/// The policy. Call [`Bell::observe`] first.
pub fn decide(bell: &Bell, facts: &Facts, config: &DoorbellConfig, now: Instant) -> Action {
    if !facts.idle || facts.draft {
        return Action::Nothing;
    }
    let due = bell
        .last_ring
        .is_none_or(|last| now >= last + config.delay_after(bell.rings));
    if let Some(id) = facts.inbox.queued.first()
        && ring_still_admitted(&facts.inbox, config, now)
        && due
    {
        return Action::Ring {
            id: id.clone(),
            pre_cr: !bell.outstanding.is_empty() || bell.cleanup_owed,
        };
    }
    if bell.cleanup_owed {
        return Action::Cleanup;
    }
    Action::Nothing
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
    /// The agent reports it waits for input (a dialog, a permission prompt).
    pub waiting_for_input: bool,
    /// A paste into the pane has not finished.
    pub paste_open: bool,
    /// The user may have an unsent draft in the pane.
    pub draft: bool,
    /// The doorbell's last try found no screen it could type into
    /// ([`DoorbellWrite::NoComposer`]).
    pub unrecognised: bool,
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
/// working; the agent waiting for input (`awaiting_prompt`); an open paste
/// (`paste_open`); an unsent draft (`possible_draft`); the last try finding
/// no screen to type into (`awaiting_prompt`); otherwise no reason, the clock
/// running.
pub fn obstacle(facts: &HoldFacts) -> (Option<crate::inbox_display::WakeHold>, HoldClock) {
    use crate::inbox_display::WakeHold;
    if !facts.queued {
        (None, HoldClock::Reset)
    } else if facts.receiver || facts.working {
        (None, HoldClock::Paused)
    } else if facts.waiting_for_input {
        (Some(WakeHold::AwaitingPrompt), HoldClock::Runs)
    } else if facts.paste_open {
        (Some(WakeHold::PasteOpen), HoldClock::Runs)
    } else if facts.draft {
        (Some(WakeHold::PossibleDraft), HoldClock::Runs)
    } else if facts.unrecognised {
        (Some(WakeHold::AwaitingPrompt), HoldClock::Runs)
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
/// screen it could type into, after a write that ended in `outcome`: a try
/// that found none holds as `awaiting_prompt` until a write goes in, and an
/// outcome that wrote nothing for another reason leaves it as it was
/// (`before`).
fn unrecognised_after(outcome: DoorbellWrite, before: bool) -> bool {
    match outcome {
        DoorbellWrite::NoComposer => true,
        DoorbellWrite::Written
        | DoorbellWrite::Flushed
        | DoorbellWrite::Failed
        | DoorbellWrite::Cancelled { .. } => false,
        DoorbellWrite::DraftPending
        | DoorbellWrite::NotOwner
        | DoorbellWrite::NotReading
        | DoorbellWrite::NotAdmitted => before,
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
    if std::env::var("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE").as_deref() != Ok(pane) {
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
    /// A write for this agent is in flight.
    busy: bool,
    /// PR #1735 fifth review (R5).
    stall: StallClock,
    /// PRD #1666 part 3, round 5-wake.
    hold: HoldEpisode,
    /// The last write found no screen it could type into
    /// ([`DoorbellWrite::NoComposer`]), and none has gone in since.
    unrecognised: bool,
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
    // the line would land in no live conversation.
    // Part 2 (`inbox/doorbell/008`): and the generation's own readiness, kept
    // apart from the Idle its card presents. One with none is rung only
    // through its agent's composer gate, and the writer checks what is left on
    // screen.
    type Admission = (
        Option<crate::state::DoorbellAdmission>,
        DoorbellReady,
        Option<crate::state::SessionStatus>,
    );
    let admissions: Vec<Admission> = {
        let state = state.read().await;
        keys.iter()
            .map(|(pane, agent)| {
                let ready = match state.doorbell_readiness(agent, pane) {
                    (_, Some(prompt)) => DoorbellReady::Screen(prompt),
                    (crate::state::ReadinessLevel::Unproven, None) => DoorbellReady::Unproven,
                    _ => DoorbellReady::Ready,
                };
                (
                    state.doorbell_admission(agent, pane),
                    ready,
                    state.agent_status(agent, pane),
                )
            })
            .collect()
    };
    #[cfg(not(feature = "e2e"))]
    let now = Instant::now();
    for ((pane, agent), (admission, ready, status)) in keys.into_iter().zip(admissions) {
        let explained = status_explains_waiting(status.as_ref());
        let input = registry.pane_input_hold(&pane);
        let facts = Facts {
            inbox: hub.doorbell_facts(&pane, &agent),
            idle: admission.is_some()
                && (!matches!(ready, DoorbellReady::Unproven)
                    || registry.has_doorbell_composer(&agent)),
            draft: input.paste_open || input.draft,
        };
        let (action, stranded) = {
            let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slots.entry((pane.clone(), agent.clone())).or_default();
            let waiting = facts.inbox.armed == 0 && !explained;
            match slot.stall.observe(
                facts.inbox.queued.first().map(String::as_str),
                waiting,
                now,
                config.wake_stall_after,
            ) {
                StallChange::Raise => hub.set_wake_stalled(&pane, &agent, true),
                StallChange::Clear => hub.set_wake_stalled(&pane, &agent, false),
                StallChange::Nothing => {}
            }
            let hold = HoldFacts {
                queued: !facts.inbox.queued.is_empty(),
                receiver: !ring_still_admitted(&facts.inbox, config, now),
                working: !matches!(
                    status,
                    Some(
                        crate::state::SessionStatus::Idle
                            | crate::state::SessionStatus::WaitingForInput
                    )
                ),
                waiting_for_input: status == Some(crate::state::SessionStatus::WaitingForInput),
                paste_open: input.paste_open,
                draft: input.draft,
                unrecognised: slot.unrecognised,
                turns: input.turns,
            };
            if let Some(shown) = slot.hold.observe(&hold, now, config.wake_stall_after) {
                info!(pane_id = %pane, agent_id = %agent, hold = ?shown, ?hold, "doorbell hold");
                hub.set_wake_hold(&pane, &agent, shown);
            }
            if slot.busy {
                continue;
            }
            slot.bell.observe(&facts);
            let action = decide(&slot.bell, &facts, config, now);
            if action == Action::Nothing {
                if slot.bell.is_idle() && facts.inbox.queued.is_empty() {
                    slots.remove(&(pane.clone(), agent.clone()));
                }
                continue;
            }
            slot.busy = true;
            (action, slot.bell.unsubmitted.clone())
        };
        let registry = Arc::clone(registry);
        let slots = Arc::clone(slots);
        let state = Arc::clone(state);
        let config = config.clone();
        // The queue as this tick's decision saw it, not as the write left it.
        let queued = facts.inbox.queued;
        tokio::spawn(async move {
            #[cfg(feature = "e2e")]
            let mut _busy_guard = TestBusyGuard {
                slots: Arc::clone(&slots),
                key: Some((pane.clone(), agent.clone())),
            };
            let (pre_cr, line) = match &action {
                Action::Ring { id, pre_cr } => (*pre_cr, Some(doorbell_line(id))),
                _ => (true, None),
            };
            #[cfg(feature = "e2e")]
            if pre_cr && line.is_some() {
                pause_rering_for_test(&pane, "decided").await;
            }
            // PRD #1666 hardening H3: re-asked under the writer before every
            // step. The same admission (nothing happened to the pane since
            // this tick) and, for a ring, still no native component armed or
            // inside the arm grace of an announcement. Part 2
            // (`inbox/doorbell/031`): and the message this ring names still
            // queued for this generation, from a fresh query each time. Other
            // queued mail does not keep it alive: a ring is never typed for a
            // message that was fetched or acknowledged since the decision.
            // The query takes and drops the hub's lock; nothing here holds it
            // across the writer's sleeps or writes.
            let selected = match &action {
                Action::Ring { id, .. } => Some(id.clone()),
                _ => None,
            };
            let still_admitted = || {
                let state = Arc::clone(&state);
                let registry = Arc::clone(&registry);
                let (pane, agent) = (pane.clone(), agent.clone());
                let config = config.clone();
                let selected = selected.clone();
                async move {
                    let now = state.read().await.doorbell_admission(&agent, &pane);
                    admission
                        .zip(now)
                        .is_some_and(|(decided, now)| decided.still_holds(&now))
                        && selected.is_none_or(|id| {
                            let facts = registry.inbox().doorbell_facts(&pane, &agent);
                            facts.queued.contains(&id)
                                && ring_still_admitted(&facts, &config, Instant::now())
                        })
                }
            };
            let outcome = registry
                .write_doorbell_when_ready(
                    &pane,
                    &agent,
                    ready,
                    pre_cr,
                    stranded.as_deref(),
                    line.as_deref(),
                    still_admitted,
                )
                .await;
            // The writer has been dropped; busy remains set so a completed
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
            slot.unrecognised = unrecognised_after(outcome, slot.unrecognised);
            match (outcome, &action) {
                // A ring that failed part-way may still sit in the box, so it
                // is treated as rung: the next one flushes it first. A ring
                // that only flushed typed no line of its own and keeps the
                // earlier one recorded: if its CR was swallowed, the line is
                // still the deck's and still stranded, and the next ring sends
                // one more lone CR. One that failed is recorded as having left
                // its own line; if it failed before typing it, an earlier line
                // still in the box no longer matches, and the composer gate
                // holds. A ring withdrawn after its line went in but before its
                // submit is likewise rung, with its line left in the box.
                (
                    DoorbellWrite::Written
                    | DoorbellWrite::Flushed
                    | DoorbellWrite::Failed
                    | DoorbellWrite::Cancelled {
                        line_typed: true, ..
                    },
                    Action::Ring { id, .. },
                ) => {
                    slot.bell.rang(Instant::now(), &queued);
                    slot.bell.unsubmitted = if outcome == DoorbellWrite::Flushed {
                        stranded
                    } else {
                        line
                    };
                    registry.inbox().note_ring(&pane, &agent);
                    match outcome {
                        DoorbellWrite::Written | DoorbellWrite::Flushed => info!(
                            pane_id = %pane,
                            agent_id = %agent,
                            message_id = %id,
                            queued_at_decision = queued.len(),
                            rings = slot.bell.rings,
                            pre_cr,
                            ?outcome,
                            "doorbell rang"
                        ),
                        DoorbellWrite::Cancelled { steps_written, .. } => info!(
                            pane_id = %pane,
                            agent_id = %agent,
                            message_id = %id,
                            queued_at_decision = queued.len(),
                            rings = slot.bell.rings,
                            pre_cr,
                            steps_written,
                            "doorbell ring withdrawn after its line was typed; its submit was withheld"
                        ),
                        _ => warn!(
                            pane_id = %pane,
                            agent_id = %agent,
                            message_id = %id,
                            queued_at_decision = queued.len(),
                            rings = slot.bell.rings,
                            pre_cr,
                            "doorbell ring failed part-way or could not be encoded"
                        ),
                    }
                }
                // Part 2 (`inbox/doorbell/031`): only the pre-ring CR went in
                // before the ring was withdrawn, so no line of this ring is in
                // the box and nothing is recorded as rung. The CR may have
                // submitted an earlier stranded line, or been swallowed, so
                // that line stays recorded and the bell owes what it owed.
                (
                    DoorbellWrite::Cancelled {
                        steps_written,
                        line_typed: false,
                    },
                    Action::Ring { id, .. },
                ) => {
                    info!(
                        pane_id = %pane,
                        agent_id = %agent,
                        message_id = %id,
                        queued_at_decision = queued.len(),
                        steps_written,
                        "doorbell ring withdrawn after its pre-ring CR; no line was typed"
                    );
                }
                (DoorbellWrite::Written, _) => {
                    slot.bell.cleaned();
                    debug!(pane_id = %pane, agent_id = %agent, "doorbell sent its cleanup CR");
                }
                (outcome, _) => {
                    debug!(pane_id = %pane, agent_id = %agent, ?outcome, "doorbell held");
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

    /// Scenario: Check the obstacle table row by row: each fact that holds a ring maps to its reason in priority order, an armed component and a working agent show none with the clock stopped, nothing queued ends the episode, and a pane with nothing in the way runs the clock with no reason. Then check that a try that found no screen to type into holds as awaiting a prompt until a write goes in, and that a try that wrote nothing for another reason leaves that as it was.
    #[spec("inbox/doorbell/041")]
    #[test]
    fn doorbell_041_the_obstacle_table_takes_the_first_row_that_holds() {
        use crate::inbox_display::WakeHold;
        let all = HoldFacts {
            queued: true,
            receiver: true,
            working: true,
            waiting_for_input: true,
            paste_open: true,
            draft: true,
            unrecognised: true,
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
            (Some(WakeHold::AwaitingPrompt), HoldClock::Runs)
        );
        let all = HoldFacts {
            unrecognised: false,
            ..all
        };
        assert_eq!(obstacle(&all), (None, HoldClock::Runs));

        assert!(unrecognised_after(DoorbellWrite::NoComposer, false));
        for wrote in [
            DoorbellWrite::Written,
            DoorbellWrite::Flushed,
            DoorbellWrite::Failed,
            DoorbellWrite::Cancelled {
                steps_written: 1,
                line_typed: false,
            },
        ] {
            assert!(!unrecognised_after(wrote, true), "{wrote:?}");
        }
        for nothing in [
            DoorbellWrite::DraftPending,
            DoorbellWrite::NotOwner,
            DoorbellWrite::NotReading,
            DoorbellWrite::NotAdmitted,
        ] {
            assert!(unrecognised_after(nothing, true), "{nothing:?}");
            assert!(!unrecognised_after(nothing, false), "{nothing:?}");
        }
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
            draft: false,
        }
    }

    fn ring(id: &str, pre_cr: bool) -> Action {
        Action::Ring {
            id: id.to_string(),
            pre_cr,
        }
    }

    fn step(bell: &mut Bell, facts: &Facts, config: &DoorbellConfig, now: Instant) -> Action {
        bell.observe(facts);
        let action = decide(bell, facts, config, now);
        if let Action::Ring { .. } = action {
            bell.rang(now, &facts.inbox.queued);
        } else if action == Action::Cleanup {
            bell.cleaned();
        }
        action
    }

    /// Scenario: Feed the policy an empty inbox, then one queued message under
    /// each guard in turn (not Idle, a draft, an armed waiter, a Native
    /// announcement inside its grace). It must ring only once every guard
    /// passes, and never with an empty queue.
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
        let mut drafted = facts(&["m1"]);
        drafted.draft = true;
        assert_eq!(decide(&bell, &drafted, &config, now), Action::Nothing);
        let mut armed = facts(&["m1"]);
        armed.inbox.armed = 1;
        assert_eq!(decide(&bell, &armed, &config, now), Action::Nothing);
        let mut native = facts(&["m1"]);
        native.inbox.native_announced_at = Some(now);
        assert_eq!(decide(&bell, &native, &config, now), Action::Nothing);
        assert_eq!(
            decide(&bell, &native, &config, now + config.arm_grace),
            ring("m1", false)
        );
        assert_eq!(
            decide(&bell, &facts(&["m1"]), &config, now),
            ring("m1", false)
        );
    }

    /// Scenario: Leave two messages queued and step the policy through time.
    /// One ring covers both and names the oldest; it re-rings after each
    /// schedule delay with a pre-ring CR while the ring is outstanding, the
    /// last delay repeats, and a fetch stops the rings and owes one cleanup CR.
    #[spec("inbox/doorbell/014")]
    #[test]
    fn doorbell_014_coalesces_reschedules_and_cleans_up() {
        let config = DoorbellConfig {
            schedule: vec![Duration::from_secs(1), Duration::from_secs(2)],
            ..DoorbellConfig::default()
        };
        let t0 = Instant::now();
        let both = facts(&["m1", "m2"]);
        let mut bell = Bell::default();
        assert_eq!(step(&mut bell, &both, &config, t0), ring("m1", false));
        let at = |ms| t0 + Duration::from_millis(ms);
        assert_eq!(step(&mut bell, &both, &config, at(999)), Action::Nothing);
        assert_eq!(step(&mut bell, &both, &config, at(1000)), ring("m1", true));
        assert_eq!(step(&mut bell, &both, &config, at(2999)), Action::Nothing);
        assert_eq!(step(&mut bell, &both, &config, at(3000)), ring("m1", true));
        assert_eq!(step(&mut bell, &both, &config, at(5000)), ring("m1", true));
        // m1 fetched: the outstanding ring is owed its flush, and m2 still
        // rings on the running schedule.
        let rest = facts(&["m2"]);
        assert_eq!(step(&mut bell, &rest, &config, at(5001)), Action::Cleanup);
        assert_eq!(step(&mut bell, &rest, &config, at(5002)), Action::Nothing);
        assert_eq!(step(&mut bell, &rest, &config, at(7000)), ring("m2", false));
        // Everything fetched: one cleanup, then silence, and the schedule
        // restarts for the next message.
        let empty = facts(&[]);
        assert_eq!(step(&mut bell, &empty, &config, at(7001)), Action::Cleanup);
        assert_eq!(step(&mut bell, &empty, &config, at(9000)), Action::Nothing);
        assert!(bell.is_idle());
        assert_eq!(
            step(&mut bell, &facts(&["m3"]), &config, at(9001)),
            ring("m3", false)
        );
    }

    /// Scenario: Owe a cleanup CR while the user holds a draft, then while the
    /// agent is busy, then with a waiter armed. The CR waits for the draft and
    /// for Idle, but an armed waiter does not hold it.
    #[spec("inbox/doorbell/015")]
    #[test]
    fn doorbell_015_cleanup_waits_for_draft_and_idle_not_for_waiters() {
        let config = DoorbellConfig::default();
        let now = Instant::now();
        let mut bell = Bell::default();
        bell.rang(now, &["m1".to_string()]);
        let mut drafted = facts(&[]);
        drafted.draft = true;
        bell.observe(&drafted);
        assert!(bell.cleanup_owed);
        assert_eq!(decide(&bell, &drafted, &config, now), Action::Nothing);
        let mut busy = facts(&[]);
        busy.idle = false;
        assert_eq!(decide(&bell, &busy, &config, now), Action::Nothing);
        let mut armed = facts(&[]);
        armed.inbox.armed = 1;
        assert_eq!(decide(&bell, &armed, &config, now), Action::Cleanup);
    }

    /// Scenario: Record a ring that left its line, then fold in a fetch, send
    /// the cleanup CR, and ring again. The line stays recorded while a CR
    /// that may submit it is owed, and is forgotten once that CR went or a
    /// new ring replaced the record.
    #[spec("inbox/doorbell/017")]
    #[test]
    fn doorbell_017_remembers_its_line_only_while_a_flush_is_owed() {
        let now = Instant::now();
        let line = doorbell_line("m1");
        let mut bell = Bell::default();
        bell.rang(now, &["m1".to_string()]);
        assert_eq!(bell.unsubmitted, None, "what a ring left is the caller's");
        bell.unsubmitted = Some(line.clone());
        bell.observe(&facts(&["m1"]));
        assert_eq!(bell.unsubmitted.as_deref(), Some(line.as_str()));
        bell.observe(&facts(&[]));
        assert!(bell.cleanup_owed);
        assert_eq!(
            bell.unsubmitted.as_deref(),
            Some(line.as_str()),
            "the owed cleanup CR may still be the one that submits it"
        );
        bell.cleaned();
        assert_eq!(bell.unsubmitted, None, "the cleanup CR went");
        bell.unsubmitted = Some(line);
        bell.rang(now, &["m2".to_string()]);
        assert_eq!(bell.unsubmitted, None, "a new ring replaces the record");
    }

    /// Scenario: Ring for two messages and record the line naming the first,
    /// then fetch the first just as a ring for the second is due. That ring
    /// goes in place of the owed cleanup, with its flushing CR; a ring that
    /// only flushed keeps the first line recorded, and each later due ring
    /// flushes again, one CR per ring, until the second message leaves
    /// `queued` and the cleanup CR drops the record.
    #[spec("inbox/doorbell/018")]
    #[test]
    fn doorbell_018_a_ring_for_other_queued_work_keeps_the_record_past_its_message() {
        let config = DoorbellConfig {
            schedule: vec![Duration::from_secs(1)],
            ..DoorbellConfig::default()
        };
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let first = doorbell_line("m1");
        let mut bell = Bell::default();
        assert_eq!(
            step(&mut bell, &facts(&["m1", "m2"]), &config, t0),
            ring("m1", false)
        );
        bell.unsubmitted = Some(first.clone());
        let rest = facts(&["m2"]);
        for due in [1000, 2000, 3000] {
            bell.observe(&rest);
            assert_eq!(
                decide(&bell, &rest, &config, at(due)),
                ring("m2", true),
                "a due ring for the remaining work flushes first, in place of the cleanup"
            );
            // The daemon's `DoorbellWrite::Flushed` arm: a ring that only
            // flushed keeps the line it found.
            bell.rang(at(due), &rest.inbox.queued);
            bell.unsubmitted = Some(first.clone());
            assert!(!bell.cleanup_owed, "the ring took the cleanup's place");
            assert_eq!(
                decide(&bell, &rest, &config, at(due + 1)),
                Action::Nothing,
                "one CR per due ring"
            );
        }
        let empty = facts(&[]);
        assert_eq!(step(&mut bell, &empty, &config, at(3001)), Action::Cleanup);
        assert_eq!(bell.unsubmitted, None, "the cleanup CR drops the record");
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
