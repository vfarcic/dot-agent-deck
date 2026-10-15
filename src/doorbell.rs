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

use crate::agent_pty::{AgentPtyRegistry, DoorbellWrite};
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
}

impl Default for DoorbellConfig {
    fn default() -> Self {
        Self {
            tick: DEFAULT_TICK,
            schedule: DEFAULT_SCHEDULE.to_vec(),
            arm_grace: DEFAULT_ARM_GRACE,
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
    /// The last hook-reported status is Idle.
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
/// again under the writer before each byte of a ring, so a component that
/// arms or announces while the ring waits for the writer stops it.
pub fn ring_still_admitted(inbox: &DoorbellFacts, config: &DoorbellConfig, now: Instant) -> bool {
    inbox.armed == 0
        && inbox
            .native_announced_at
            .is_none_or(|at| now >= at + config.arm_grace)
}

type AgentKey = (String, String);

#[derive(Default)]
struct Slot {
    bell: Bell,
    /// A write for this agent is in flight.
    busy: bool,
}

/// Run the doorbell for as long as the returned task lives.
pub fn spawn(
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    config: DoorbellConfig,
) -> tokio::task::JoinHandle<()> {
    let slots: Arc<Mutex<HashMap<AgentKey, Slot>>> = Arc::default();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(config.tick);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            run_tick(&registry, &state, &config, &slots).await;
        }
    })
}

async fn run_tick(
    registry: &Arc<AgentPtyRegistry>,
    state: &SharedState,
    config: &DoorbellConfig,
    slots: &Arc<Mutex<HashMap<AgentKey, Slot>>>,
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
    let admissions: Vec<Option<crate::state::DoorbellAdmission>> = {
        let state = state.read().await;
        keys.iter()
            .map(|(pane, agent)| state.doorbell_admission(agent, pane))
            .collect()
    };
    let now = Instant::now();
    for ((pane, agent), admission) in keys.into_iter().zip(admissions) {
        let facts = Facts {
            inbox: hub.doorbell_facts(&pane, &agent),
            idle: admission.is_some(),
            draft: registry.draft_pending(&pane),
        };
        let (action, stranded) = {
            let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slots.entry((pane.clone(), agent.clone())).or_default();
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
        let queued = facts.inbox.queued;
        tokio::spawn(async move {
            let (pre_cr, line) = match &action {
                Action::Ring { id, pre_cr } => (*pre_cr, Some(doorbell_line(id))),
                _ => (true, None),
            };
            // PRD #1666 hardening H3: re-asked under the writer before every
            // byte. The same admission (nothing happened to the pane since
            // this tick) and, for a ring, still no native component armed or
            // inside the arm grace of an announcement.
            let ring = matches!(action, Action::Ring { .. });
            let still_admitted = || {
                let state = Arc::clone(&state);
                let registry = Arc::clone(&registry);
                let (pane, agent) = (pane.clone(), agent.clone());
                let config = config.clone();
                async move {
                    admission.is_some()
                        && state.read().await.doorbell_admission(&agent, &pane) == admission
                        && (!ring
                            || ring_still_admitted(
                                &registry.inbox().doorbell_facts(&pane, &agent),
                                &config,
                                Instant::now(),
                            ))
                }
            };
            let outcome = registry
                .write_doorbell(
                    &pane,
                    &agent,
                    pre_cr,
                    stranded.as_deref(),
                    line.as_deref(),
                    still_admitted,
                )
                .await;
            let mut slots = slots.lock().unwrap_or_else(|e| e.into_inner());
            let Some(slot) = slots.get_mut(&(pane.clone(), agent.clone())) else {
                return;
            };
            slot.busy = false;
            match (outcome, &action) {
                // A ring that failed part-way may still sit in the box, so it
                // is treated as rung: the next one flushes it first. A ring
                // that only flushed typed no line of its own and keeps the
                // earlier one recorded: if its CR was swallowed, the line is
                // still the deck's and still stranded, and the next ring sends
                // one more lone CR. One that failed is recorded as having left
                // its own line; if it failed before typing it, an earlier line
                // still in the box no longer matches, and the composer gate
                // holds.
                (
                    DoorbellWrite::Written | DoorbellWrite::Flushed | DoorbellWrite::Failed,
                    Action::Ring { id, .. },
                ) => {
                    slot.bell.rang(Instant::now(), &queued);
                    slot.bell.unsubmitted = if outcome == DoorbellWrite::Flushed {
                        stranded
                    } else {
                        line
                    };
                    registry.inbox().note_ring(&pane, &agent);
                    info!(
                        pane_id = %pane,
                        agent_id = %agent,
                        message_id = %id,
                        queued = queued.len(),
                        rings = slot.bell.rings,
                        pre_cr,
                        ?outcome,
                        "doorbell rang"
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

    fn facts(queued: &[&str]) -> Facts {
        Facts {
            inbox: DoorbellFacts {
                queued: queued.iter().map(|s| s.to_string()).collect(),
                armed: 0,
                native_announced_at: None,
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
