//! PRD #1589: the deck's record of the units `dispatch` started — who
//! dispatched each one, what it is, where it lives, whether it has reported,
//! and how it ended.
//!
//! The dispatch RETURN edge ([`crate::dispatch_return`]) forgets the caller the
//! moment the unit reports `work-done --done`, because delivering that report
//! is its whole job. That is exactly the moment a dispatcher wants to close the
//! unit, so the return route cannot be the provenance a close is authorized
//! from. This module is that provenance: one record per unit, kept from before
//! the unit's task is delivered until every pane of it is gone, then a bounded
//! tombstone for diagnostics.
//!
//! Named for units rather than for closing, because the sibling PRD #1590
//! (follow-up work for a unit) resolves and authorizes against the same record.
//! Invariant recorded for it here: a follow-up that delivers new work into a
//! unit must reset [`DispatchedUnit::completed_at_ms`] before the delivery,
//! because a completion mark from an earlier task says nothing about a later
//! one.
//!
//! Like [`crate::dispatch_return`] it is pure data: no locks, no I/O and no
//! registry access, so every rule is asserted directly rather than through
//! PTYs. [`crate::agent_pty::AgentPtyRegistry`] owns one behind its own mutex.
//!
//! **Memory-only.** After a daemon restart nothing is recorded, every name
//! resolves to [`RefusalReason::UnknownUnit`], and a unit id minted by the
//! previous daemon is recognised as such (its epoch differs) so the refusal can
//! say why. A persisted record could never be matched to an agent caller anyway:
//! it would name a dispatcher agent id the new daemon never minted.

use std::collections::VecDeque;
use std::path::PathBuf;

use crate::state::SessionStatus;

/// The most tombstones kept. A tombstone only turns "unknown unit" into
/// "already ended" for a name someone typed late, so a short history covers the
/// case that matters (a dispatcher closing what it just saw finish) without
/// growing for the daemon's lifetime.
pub const MAX_TOMBSTONES: usize = 64;

/// The most characters of a unit's dispatch name the record retains, and that a
/// tombstone's text fields keep. The same bound and the same reasoning as
/// `crate::config_validation::MAX_QUOTED_VALUE_CHARS`: a unit name is a slug a
/// caller typed, so 120 characters is far past any real one while leaving an
/// absurd one unable to grow the record.
pub const MAX_RECORDED_TEXT_CHARS: usize = 120;

/// Why a close was refused, on the wire and in the CLI's output.
///
/// Kebab-case on the wire, with `#[serde(other)] Unknown` so a reason a newer
/// daemon adds decodes in an older client instead of failing the whole reply —
/// the same shape as [`crate::daemon_protocol::StopRefusalReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalReason {
    /// The target is (part of) a dispatched unit that has not reported an
    /// attested terminal `work-done --done`. Forceable.
    NotReported,
    /// A targeted pane is mid-turn: Thinking, Working, Compacting,
    /// WaitingForInput or Blocked. Forceable.
    Busy,
    /// `--pane` names one role of a live orchestration, which would strand the
    /// rest of it. Forceable.
    StrandsOrchestration,
    /// The caller presented a pane claim whose token does not attest it. Never
    /// forceable.
    NotAttested,
    /// The caller's token was attested, but the generation it names no longer
    /// holds its pane. Never forceable.
    Superseded,
    /// The target was not dispatched by the calling agent. Never forceable.
    NotYourUnit,
    /// The target is the caller's own pane, or a pane of its own orchestration.
    /// Never forceable.
    OwnPane,
    /// No live unit matches the name or id.
    UnknownUnit,
    /// The name matches more than one live unit.
    Ambiguous,
    /// The unit already ended.
    AlreadyEnded,
    /// A bulk selector was sent without `dry_run`; bulk closes apply by
    /// explicit unit ids.
    BulkRequiresDryRun,
    /// No agent holds the named pane.
    UnknownPane,
    /// `--orchestration-of` named a pane that is not an orchestration role.
    NotAnOrchestration,
    /// The selector names more entries, or longer ones, than one request may
    /// (`crate::close_agents::MAX_SELECTOR_ENTRIES` and its siblings). The
    /// whole request is refused before anything is resolved or stopped.
    SelectorTooLarge,
    /// A reason from a newer daemon this build does not know.
    #[serde(other)]
    Unknown,
}

impl RefusalReason {
    /// Whether `--force` overrides this refusal. Authority and resolution never
    /// are.
    pub fn forceable(self) -> bool {
        matches!(
            self,
            RefusalReason::NotReported | RefusalReason::Busy | RefusalReason::StrandsOrchestration
        )
    }

    /// The kebab-case spelling, the same one the wire carries.
    pub fn code(self) -> &'static str {
        match self {
            RefusalReason::NotReported => "not-reported",
            RefusalReason::Busy => "busy",
            RefusalReason::StrandsOrchestration => "strands-orchestration",
            RefusalReason::NotAttested => "not-attested",
            RefusalReason::Superseded => "superseded",
            RefusalReason::NotYourUnit => "not-your-unit",
            RefusalReason::OwnPane => "own-pane",
            RefusalReason::UnknownUnit => "unknown-unit",
            RefusalReason::Ambiguous => "ambiguous",
            RefusalReason::AlreadyEnded => "already-ended",
            RefusalReason::BulkRequiresDryRun => "bulk-requires-dry-run",
            RefusalReason::UnknownPane => "unknown-pane",
            RefusalReason::NotAnOrchestration => "not-an-orchestration",
            RefusalReason::SelectorTooLarge => "selector-too-large",
            RefusalReason::Unknown => "unknown",
        }
    }
}

/// The pane that ran `dispatch`, as the record keeps it: the pair captured
/// from the attested caller's own record at dispatch time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatcher {
    pub pane_id: String,
    pub agent_id: String,
}

/// What a unit is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitKind {
    /// One agent, in one pane.
    Single { pane_id: String, agent_id: String },
    /// An orchestration instance. Membership is the instance token, which every
    /// role carries in its registry record — so it stays right across
    /// `pane spawn` and `clear = true` respawns, where a frozen list of agent
    /// ids would not. The terminal pane and generation are the ones a
    /// completion must come from: the orchestrator role the task went to.
    ///
    /// **Invariant (PRD #1589 D5):** a respawn of the terminal generation — a
    /// new agent replacing the orchestrator in its pane while the instance
    /// lives on — must move `terminal_agent_id` to the replacement, or
    /// [`DispatchedUnits::mark_completed`] stops matching and the unit can then
    /// only be closed with `--force`. The registry's respawn seam does that
    /// ([`DispatchedUnits::note_generation_replaced`]). No user-facing route
    /// respawns the orchestrator role today — `clear = true` delegation and
    /// `pane restart` target workers only, and `pane spawn` refuses the start
    /// role — but the low-level registry respawn
    /// (`AgentPtyRegistry::respawn_agent_for_pane`) can replace any pane, and
    /// a route that replaces the orchestrator by any other means owes the same
    /// update.
    Orchestration {
        orchestration_id: String,
        name: String,
        terminal_pane_id: String,
        terminal_agent_id: String,
    },
}

impl UnitKind {
    /// The `(pane, agent id)` a completion report must come from to mark this
    /// unit complete.
    pub fn terminal(&self) -> (&str, &str) {
        match self {
            UnitKind::Single { pane_id, agent_id } => (pane_id, agent_id),
            UnitKind::Orchestration {
                terminal_pane_id,
                terminal_agent_id,
                ..
            } => (terminal_pane_id, terminal_agent_id),
        }
    }

    /// `"single"` or `"orchestration"`, for listings.
    pub fn label(&self) -> &'static str {
        match self {
            UnitKind::Single { .. } => "single",
            UnitKind::Orchestration { .. } => "orchestration",
        }
    }

    /// The orchestration instance token, for an orchestration unit.
    pub fn orchestration_id(&self) -> Option<&str> {
        match self {
            UnitKind::Single { .. } => None,
            UnitKind::Orchestration {
                orchestration_id, ..
            } => Some(orchestration_id),
        }
    }
}

/// Where a live unit is in its life.
///
/// A unit is removed from the live set — and becomes a [`Tombstone`] — only
/// once its records are gone and the stop outcome is known, so `Ended` is not a
/// state a live record is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnitState {
    /// At least one of its agents is running.
    Running,
    /// Every agent exited on its own, but the records are still registered:
    /// the unit is still closeable (with `--force` if it never reported), and
    /// closing it is what triggers its worktree cleanup.
    Exited,
    /// A close is taking it down.
    Closing,
    /// A state from a newer daemon this build does not know.
    #[serde(other)]
    Unknown,
}

/// How a unit ended, kept on its tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndReason {
    /// Closed through the `close` verb.
    ClosedByVerb,
    /// Closed through an ordinary stop: the TUI, the desktop app, or a raw
    /// `StopAgent`.
    Stopped,
    #[serde(other)]
    Unknown,
}

/// What [`DispatchedUnits::register`] needs to record a unit.
#[derive(Debug, Clone)]
pub struct NewUnit {
    pub name: String,
    pub worktree: PathBuf,
    pub branch: String,
    pub clone_dir: PathBuf,
    pub dispatcher: Dispatcher,
    pub kind: UnitKind,
    pub dispatched_at_ms: i64,
}

/// Everything about a unit that is known before its spawn: what `dispatch`
/// hands [`crate::spawn::spawn_dispatched_unit`], which adds the kind once the
/// spawn's identities exist and registers the unit before its task is
/// delivered.
#[derive(Debug, Clone)]
pub struct UnitOrigin {
    pub name: String,
    pub worktree: PathBuf,
    pub branch: String,
    pub clone_dir: PathBuf,
    pub dispatcher: Dispatcher,
}

impl UnitOrigin {
    /// The [`NewUnit`] for this origin once the spawn says what it opened.
    pub fn into_unit(self, kind: UnitKind, dispatched_at_ms: i64) -> NewUnit {
        NewUnit {
            name: self.name,
            worktree: self.worktree,
            branch: self.branch,
            clone_dir: self.clone_dir,
            dispatcher: self.dispatcher,
            kind,
            dispatched_at_ms,
        }
    }
}

/// One live dispatched unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchedUnit {
    /// Daemon-lifetime-unique, never reused: `u-<epoch>-<n>`.
    pub id: String,
    /// The name the dispatcher passed, bounded to [`MAX_RECORDED_TEXT_CHARS`].
    /// Producer input: escape it before it reaches a terminal or a log.
    pub name: String,
    /// The sanitized slug the worktree path and branch were derived from, and
    /// the key names are resolved by.
    pub slug: String,
    pub worktree: PathBuf,
    pub branch: String,
    pub clone_dir: PathBuf,
    pub dispatcher: Dispatcher,
    pub kind: UnitKind,
    pub dispatched_at_ms: i64,
    /// When an attested terminal `work-done --done` from the authorized
    /// terminal generation arrived. `None` until then.
    pub completed_at_ms: Option<i64>,
    pub state: UnitState,
}

/// What [`DispatchedUnits`] keeps of an ended unit: enough to say "already
/// ended" to the right caller, never an authorization source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub id: String,
    pub slug: String,
    pub clone_dir: String,
    pub dispatcher_agent_id: String,
    pub kind: &'static str,
    pub reason: EndReason,
    pub ended_at_ms: i64,
}

/// Who is asking, as the daemon established it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// No pane claim: a person at a shell, the TUI, the desktop. Same reach as
    /// the clients' own close.
    Person,
    /// An attested, still-current agent generation.
    Agent {
        pane_id: String,
        agent_id: String,
        /// The orchestration instance the caller's own pane belongs to, if any.
        orchestration_id: Option<String>,
    },
}

/// A resolution or authority refusal, with the sentence that explains it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub reason: RefusalReason,
    pub message: String,
    /// For [`RefusalReason::Ambiguous`]: the ids of every candidate.
    pub candidates: Vec<String>,
}

impl Refused {
    fn new(reason: RefusalReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
            candidates: Vec::new(),
        }
    }
}

/// The slug a dispatch name resolves by — the same transformation the worktree
/// path and branch were derived with, so `issue 1531` and `issue-1531` name the
/// same unit.
pub fn slug_of(name: &str) -> String {
    crate::dispatch::sanitize_name(name)
}

fn bounded(s: &str) -> String {
    s.chars().take(MAX_RECORDED_TEXT_CHARS).collect()
}

/// Every unit the daemon dispatched and has not seen end, plus a bounded
/// history of the ones that did.
#[derive(Debug)]
pub struct DispatchedUnits {
    /// Random per daemon, so an id from a previous daemon never matches a unit
    /// of this one, and can be recognised as foreign.
    epoch: String,
    next_seq: u64,
    live: Vec<DispatchedUnit>,
    tombstones: VecDeque<Tombstone>,
}

/// How many hex characters of OS randomness a daemon's epoch keeps: 64 bits.
///
/// The epoch is what stops a unit id a previous daemon issued — one a script
/// held across `daemon restart` — from naming a unit of this daemon, whose
/// counter starts at 1 again. With the six characters it started with, two
/// epochs matched one time in about 16.7 million (auditor B4 note); at 64 bits
/// a match is not a case that occurs.
pub const EPOCH_HEX_CHARS: usize = 16;

impl Default for DispatchedUnits {
    fn default() -> Self {
        let token = crate::hook_provenance::mint();
        Self::with_epoch(&token[..EPOCH_HEX_CHARS])
    }
}

impl DispatchedUnits {
    /// A record whose ids carry `epoch`. Tests pin it; the daemon uses
    /// [`Default`].
    pub fn with_epoch(epoch: &str) -> Self {
        Self {
            epoch: epoch.to_string(),
            next_seq: 1,
            live: Vec::new(),
            tombstones: VecDeque::new(),
        }
    }

    /// Record a unit and return its new id. Called once the spawn's identities
    /// exist and before its task is delivered (PRD #1589 D5), so a unit that
    /// reports immediately already has a record to mark.
    pub fn register(&mut self, unit: NewUnit) -> String {
        let id = format!("u-{}-{}", self.epoch, self.next_seq);
        self.next_seq += 1;
        let slug = slug_of(&unit.name);
        self.live.push(DispatchedUnit {
            id: id.clone(),
            name: bounded(&unit.name),
            slug,
            worktree: unit.worktree,
            branch: unit.branch,
            clone_dir: unit.clone_dir,
            dispatcher: unit.dispatcher,
            kind: unit.kind,
            dispatched_at_ms: unit.dispatched_at_ms,
            completed_at_ms: None,
            state: UnitState::Running,
        });
        id
    }

    pub fn get(&self, id: &str) -> Option<&DispatchedUnit> {
        self.live.iter().find(|u| u.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut DispatchedUnit> {
        self.live.iter_mut().find(|u| u.id == id)
    }

    /// Every live unit, in dispatch order.
    pub fn live(&self) -> impl Iterator<Item = &DispatchedUnit> {
        self.live.iter()
    }

    pub fn tombstones(&self) -> impl Iterator<Item = &Tombstone> {
        self.tombstones.iter()
    }

    /// Whether `id` has the shape of a unit id minted by some OTHER daemon —
    /// the "the daemon restarted" hint for a stale id.
    pub fn is_foreign_id(&self, id: &str) -> bool {
        id.starts_with("u-") && !id.starts_with(&format!("u-{}-", self.epoch))
    }

    /// PRD #1589 D5: mark complete the unit whose authorized terminal
    /// generation is `(pane_id, sender_agent_id)`.
    ///
    /// `sender_agent_id` must be the generation the hook gate ATTESTED for the
    /// report — never the pane's current occupant looked up afterwards, which
    /// would launder a predecessor's late report into its successor, and never
    /// the record's own id, which would compare the record to itself. A worker
    /// role of an orchestration never matches, because only the terminal
    /// generation does. Idempotent: a duplicate report keeps the first time.
    /// Returns the unit id it marked, if any.
    ///
    /// Compares against the terminal generation recorded at registration; see
    /// the invariant on [`UnitKind::Orchestration`] for the respawn that would
    /// have to update it.
    pub fn mark_completed(
        &mut self,
        pane_id: &str,
        sender_agent_id: &str,
        now_ms: i64,
    ) -> Option<String> {
        let unit = self
            .live
            .iter_mut()
            .find(|u| u.kind.terminal() == (pane_id, sender_agent_id))?;
        if unit.completed_at_ms.is_none() {
            unit.completed_at_ms = Some(now_ms);
        }
        Some(unit.id.clone())
    }

    /// PRD #1589 D5: the generation `old_agent_id` in `pane_id` was replaced
    /// in place by `new_agent_id` (a respawn). A unit bound to the old
    /// generation — a single unit that IS it, or an orchestration whose
    /// terminal generation it was — follows it, so the replacement's attested
    /// completion still marks the unit and a close still reaches it. The
    /// completion mark is kept: a respawn is not new work (see the module
    /// note for the follow-up that is). Returns the unit id it moved, if any.
    pub fn note_generation_replaced(
        &mut self,
        pane_id: &str,
        old_agent_id: &str,
        new_agent_id: &str,
    ) -> Option<String> {
        let unit = self.live.iter_mut().find(|u| match &u.kind {
            UnitKind::Single {
                pane_id: p,
                agent_id: a,
            } => p == pane_id && a == old_agent_id,
            UnitKind::Orchestration {
                terminal_pane_id: p,
                terminal_agent_id: a,
                ..
            } => p == pane_id && a == old_agent_id,
        })?;
        match &mut unit.kind {
            UnitKind::Single { agent_id, .. } => *agent_id = new_agent_id.to_string(),
            UnitKind::Orchestration {
                terminal_agent_id, ..
            } => *terminal_agent_id = new_agent_id.to_string(),
        }
        Some(unit.id.clone())
    }

    /// The live unit the generation `agent_id` in `pane_id` belongs to: the
    /// single unit that IS that generation, or the orchestration unit whose
    /// instance token the generation's record carries.
    pub fn unit_of_generation(
        &self,
        pane_id: Option<&str>,
        agent_id: &str,
        orchestration_id: Option<&str>,
    ) -> Option<&DispatchedUnit> {
        self.live.iter().find(|u| match &u.kind {
            UnitKind::Single {
                pane_id: unit_pane,
                agent_id: unit_agent,
            } => unit_agent == agent_id && pane_id.is_none_or(|p| p == unit_pane),
            UnitKind::Orchestration {
                orchestration_id: id,
                ..
            } => orchestration_id == Some(id.as_str()),
        })
    }

    /// The live unit of an orchestration instance.
    pub fn unit_of_instance(&self, orchestration_id: &str) -> Option<&DispatchedUnit> {
        self.live
            .iter()
            .find(|u| u.kind.orchestration_id() == Some(orchestration_id))
    }

    /// Resolve a dispatch name to one live unit for `caller`.
    ///
    /// Live units only: a tombstone produces [`RefusalReason::AlreadyEnded`],
    /// never a target. For an agent, the caller's own units are preferred over
    /// a sibling's of the same name, and only the caller's own tombstones are
    /// consulted, so a sibling's old unit never masks this caller's. A name the
    /// caller does not own but a sibling does is [`RefusalReason::NotYourUnit`].
    pub fn resolve_name(&self, name: &str, caller: &Caller) -> Result<&DispatchedUnit, Refused> {
        let slug = slug_of(name);
        let matches: Vec<&DispatchedUnit> = self.live.iter().filter(|u| u.slug == slug).collect();
        let candidates: Vec<&DispatchedUnit> = match caller {
            Caller::Person => matches.clone(),
            Caller::Agent { agent_id, .. } => matches
                .iter()
                .copied()
                .filter(|u| &u.dispatcher.agent_id == agent_id)
                .collect(),
        };
        match candidates.as_slice() {
            [one] => return Ok(one),
            [] => {}
            many => {
                let mut refused = Refused::new(
                    RefusalReason::Ambiguous,
                    format!(
                        "{} live units are named this; close one by its unit id",
                        many.len()
                    ),
                );
                refused.candidates = many.iter().map(|u| u.id.clone()).collect();
                return Err(refused);
            }
        }
        if !matches.is_empty() {
            // Only reachable for an agent: a person's candidates are every match.
            return Err(Refused::new(
                RefusalReason::NotYourUnit,
                "that unit was not dispatched by this agent",
            ));
        }
        let owner = match caller {
            Caller::Person => None,
            Caller::Agent { agent_id, .. } => Some(agent_id.as_str()),
        };
        if let Some(stone) = self
            .tombstones
            .iter()
            .rev()
            .find(|t| t.slug == slug && owner.is_none_or(|o| t.dispatcher_agent_id == o))
        {
            return Err(already_ended(stone));
        }
        Err(Refused::new(
            RefusalReason::UnknownUnit,
            "no live unit has that name (units are not remembered across a daemon restart)",
        ))
    }

    /// Resolve a daemon-issued unit id, as `close --all --yes` and `close
    /// --unit-id` apply them.
    /// Authority is checked separately ([`authorize`]); this only finds it.
    pub fn resolve_id(&self, id: &str) -> Result<&DispatchedUnit, Refused> {
        if let Some(unit) = self.get(id) {
            return Ok(unit);
        }
        if let Some(stone) = self.tombstones.iter().rev().find(|t| t.id == id) {
            return Err(already_ended(stone));
        }
        let message = if self.is_foreign_id(id) {
            "that unit id was issued by a daemon that has since restarted; nothing it \
             recorded is known to this one"
        } else {
            "no live unit has that id"
        };
        Err(Refused::new(RefusalReason::UnknownUnit, message))
    }

    /// Units dispatched by any of `agent_ids` — the descendants a close of a
    /// unit whose panes run those agents leaves open. Not closed by it: closing
    /// is not transitive.
    pub fn open_descendants<'a>(
        &'a self,
        agent_ids: &'a [String],
    ) -> impl Iterator<Item = &'a DispatchedUnit> + 'a {
        self.live
            .iter()
            .filter(move |u| agent_ids.contains(&u.dispatcher.agent_id))
    }

    /// Every agent's natural exit was seen: `Running` becomes `Exited`. A
    /// closing unit stays closing. Never tombstones.
    pub fn note_exited(&mut self, id: &str) {
        if let Some(unit) = self.get_mut(id)
            && unit.state == UnitState::Running
        {
            unit.state = UnitState::Exited;
        }
    }

    /// Start closing a unit. Returns the state to restore if the close is
    /// abandoned, or `None` when it is already closing (or gone).
    pub fn begin_closing(&mut self, id: &str) -> Option<UnitState> {
        let unit = self.get_mut(id)?;
        if unit.state == UnitState::Closing {
            return None;
        }
        let prior = unit.state;
        unit.state = UnitState::Closing;
        Some(prior)
    }

    /// Abandon a close begun with [`Self::begin_closing`], restoring `prior`.
    pub fn abort_closing(&mut self, id: &str, prior: UnitState) {
        if let Some(unit) = self.get_mut(id)
            && unit.state == UnitState::Closing
        {
            unit.state = prior;
        }
    }

    /// The unit's records are gone and its stop outcome is known: move it to
    /// the tombstones. Returns the tombstone, or `None` if it was not live.
    pub fn end(&mut self, id: &str, reason: EndReason, now_ms: i64) -> Option<Tombstone> {
        let pos = self.live.iter().position(|u| u.id == id)?;
        let unit = self.live.remove(pos);
        let stone = Tombstone {
            id: unit.id,
            slug: bounded(&unit.slug),
            clone_dir: bounded(&unit.clone_dir.to_string_lossy()),
            dispatcher_agent_id: unit.dispatcher.agent_id,
            kind: unit.kind.label(),
            reason,
            ended_at_ms: now_ms,
        };
        self.tombstones.push_back(stone.clone());
        while self.tombstones.len() > MAX_TOMBSTONES {
            self.tombstones.pop_front();
        }
        Some(stone)
    }

    #[cfg(test)]
    fn live_len(&self) -> usize {
        self.live.len()
    }
}

fn already_ended(stone: &Tombstone) -> Refused {
    let how = match stone.reason {
        EndReason::ClosedByVerb => "closed with `close`",
        EndReason::Stopped => "closed from the TUI, the desktop app or a stop",
        EndReason::Unknown => "ended",
    };
    Refused::new(
        RefusalReason::AlreadyEnded,
        format!("that unit already ended ({how})"),
    )
}

/// PRD #1589 D2: may `caller` close `unit`?
///
/// A person may close anything a client could. An agent may close only units
/// its own (attested, still-current) generation dispatched — compared by agent
/// id, not pane id, so a new occupant of the dispatcher's pane inherits
/// nothing — and never its own pane or a pane of its own orchestration. Not
/// transitive: a unit dispatched by a unit belongs to that unit's agent.
pub fn authorize(caller: &Caller, unit: &DispatchedUnit) -> Result<(), Refused> {
    let Caller::Agent {
        pane_id,
        agent_id,
        orchestration_id,
    } = caller
    else {
        return Ok(());
    };
    let own = match &unit.kind {
        UnitKind::Single {
            pane_id: unit_pane, ..
        } => unit_pane == pane_id,
        UnitKind::Orchestration {
            orchestration_id: id,
            ..
        } => orchestration_id.as_deref() == Some(id.as_str()),
    };
    if own {
        return Err(Refused::new(
            RefusalReason::OwnPane,
            "an agent cannot close its own pane or its own orchestration",
        ));
    }
    if &unit.dispatcher.agent_id != agent_id {
        return Err(Refused::new(
            RefusalReason::NotYourUnit,
            "that unit was not dispatched by this agent",
        ));
    }
    Ok(())
}

/// The statuses PRD #1589 treats as mid-turn. `Idle`, `Error`, `Unknown` and
/// "no session yet" are not refused — `Unknown` is uncertainty, not proof of
/// idle, and the docs say the status check is a measured signal rather than a
/// complete busy detector.
pub fn is_busy(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Thinking
            | SessionStatus::Working
            | SessionStatus::Compacting
            | SessionStatus::WaitingForInput
            | SessionStatus::Blocked
    )
}

/// The facts the forceable refusals are decided from, for one resolved target.
#[derive(Debug, Clone, Default)]
pub struct TargetFacts {
    /// `Some(completed)` when the target is (part of) a recorded dispatched
    /// unit; `None` for an ordinary pane or orchestration.
    pub unit_reported: Option<bool>,
    /// The status of each targeted pane that has a session.
    pub statuses: Vec<SessionStatus>,
    /// `--pane` named one role of an orchestration with other live roles.
    pub strands_orchestration: bool,
}

/// The default (forceable) refusals that apply to a target, in the order they
/// are reported, each with its detail. The same answer whatever selector
/// spelling reached the target, because it is computed from the resolved
/// target alone (auditor B4).
pub fn default_refusals(facts: &TargetFacts) -> Vec<(RefusalReason, String)> {
    let mut out = Vec::new();
    if facts.unit_reported == Some(false) {
        out.push((
            RefusalReason::NotReported,
            "had not reported `work-done --done`".to_string(),
        ));
    }
    if let Some(status) = facts.statuses.iter().find(|s| is_busy(s)) {
        out.push((RefusalReason::Busy, format!("was {status:?}")));
    }
    if facts.strands_orchestration {
        out.push((
            RefusalReason::StrandsOrchestration,
            "is one role of a live orchestration; close it whole with --orchestration-of"
                .to_string(),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(name: &str, dispatcher: &str, pane: &str, agent: &str) -> NewUnit {
        NewUnit {
            name: name.to_string(),
            worktree: PathBuf::from(format!("/repo-dispatch-{name}")),
            branch: format!("agent/dispatch-{name}"),
            clone_dir: PathBuf::from("/repo"),
            dispatcher: Dispatcher {
                pane_id: format!("{dispatcher}-pane"),
                agent_id: dispatcher.to_string(),
            },
            kind: UnitKind::Single {
                pane_id: pane.to_string(),
                agent_id: agent.to_string(),
            },
            dispatched_at_ms: 1,
        }
    }

    fn orchestration(name: &str, dispatcher: &str, orch: &str, pane: &str, agent: &str) -> NewUnit {
        NewUnit {
            kind: UnitKind::Orchestration {
                orchestration_id: orch.to_string(),
                name: "team".to_string(),
                terminal_pane_id: pane.to_string(),
                terminal_agent_id: agent.to_string(),
            },
            ..single(name, dispatcher, pane, agent)
        }
    }

    fn agent(id: &str) -> Caller {
        Caller::Agent {
            pane_id: format!("{id}-pane"),
            agent_id: id.to_string(),
            orchestration_id: None,
        }
    }

    #[test]
    fn names_resolve_by_slug_against_live_units_only() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let id = units.register(single("issue 1531", "a", "p1", "1"));
        let found = units
            .resolve_name("issue-1531", &Caller::Person)
            .expect("the slug resolves");
        assert_eq!(found.id, id);
        units.end(&id, EndReason::Stopped, 5);
        let refused = units
            .resolve_name("issue 1531", &Caller::Person)
            .expect_err("an ended unit is never a target");
        assert_eq!(refused.reason, RefusalReason::AlreadyEnded);
        let refused = units
            .resolve_name("never-dispatched", &Caller::Person)
            .expect_err("unknown");
        assert_eq!(refused.reason, RefusalReason::UnknownUnit);
    }

    #[test]
    fn a_name_in_two_clones_is_ambiguous_and_lists_both_ids() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let first = units.register(single("fix", "a", "p1", "1"));
        let mut other = single("fix", "a", "p2", "2");
        other.clone_dir = PathBuf::from("/other-repo");
        let second = units.register(other);
        let refused = units
            .resolve_name("fix", &Caller::Person)
            .expect_err("two live units share the slug");
        assert_eq!(refused.reason, RefusalReason::Ambiguous);
        assert_eq!(refused.candidates, vec![first, second]);
        // The same for the agent that dispatched both.
        let refused = units
            .resolve_name("fix", &agent("a"))
            .expect_err("ambiguous");
        assert_eq!(refused.reason, RefusalReason::Ambiguous);
    }

    #[test]
    fn an_agent_resolves_its_own_unit_over_a_siblings_of_the_same_name() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let _theirs = units.register(single("fix", "b", "p1", "1"));
        let mine = units.register(single("fix", "a", "p2", "2"));
        assert_eq!(units.resolve_name("fix", &agent("a")).unwrap().id, mine);
        // A sibling's name is NotYourUnit, not unknown.
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        units.register(single("theirs", "b", "p1", "1"));
        let refused = units
            .resolve_name("theirs", &agent("a"))
            .expect_err("not mine");
        assert_eq!(refused.reason, RefusalReason::NotYourUnit);
    }

    #[test]
    fn tombstone_lookup_is_owner_scoped_for_an_agent() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let theirs = units.register(single("fix", "b", "p1", "1"));
        units.end(&theirs, EndReason::ClosedByVerb, 2);
        // A sibling's old unit never answers "already ended" to this caller.
        let refused = units.resolve_name("fix", &agent("a")).expect_err("none");
        assert_eq!(refused.reason, RefusalReason::UnknownUnit);
        // But its owner, and a person, see it ended.
        assert_eq!(
            units.resolve_name("fix", &agent("b")).unwrap_err().reason,
            RefusalReason::AlreadyEnded
        );
        assert_eq!(
            units
                .resolve_name("fix", &Caller::Person)
                .unwrap_err()
                .reason,
            RefusalReason::AlreadyEnded
        );
    }

    #[test]
    fn a_new_unit_with_an_old_name_never_matches_the_old_id_or_tombstone() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let old = units.register(single("fix", "a", "p1", "1"));
        units.end(&old, EndReason::ClosedByVerb, 2);
        let new = units.register(single("fix", "a", "p2", "2"));
        assert_ne!(old, new, "ids are never reused");
        assert_eq!(
            units.resolve_id(&old).unwrap_err().reason,
            RefusalReason::AlreadyEnded,
            "the old preview id does not retarget to the new unit"
        );
        assert_eq!(units.resolve_id(&new).unwrap().id, new);
        assert_eq!(units.resolve_name("fix", &agent("a")).unwrap().id, new);
    }

    /// Scenario: two records built the way the daemon builds them carry
    /// epochs of [`EPOCH_HEX_CHARS`] random hex characters each, so a unit id
    /// one daemon issued is foreign to the other rather than resolving there.
    #[test]
    fn a_daemon_epoch_is_sixty_four_bits_of_randomness() {
        let mut first = DispatchedUnits::default();
        let second = DispatchedUnits::default();
        let id = first.register(single("fix", "a", "p1", "1"));
        let epoch = id
            .strip_prefix("u-")
            .and_then(|rest| rest.rsplit_once('-'))
            .map(|(epoch, _)| epoch)
            .expect("u-<epoch>-<n>");
        assert_eq!(epoch.len(), EPOCH_HEX_CHARS, "{id}");
        assert!(epoch.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        assert!(second.is_foreign_id(&id), "{id}");
    }

    /// Scenario: the orchestrator a unit's task went to is replaced in its
    /// pane. The replacement's attested completion marks the unit and the old
    /// generation's no longer does; a single unit follows its agent the same
    /// way, and a replacement in some other pane moves nothing.
    #[test]
    fn a_replaced_terminal_generation_moves_the_unit_with_it() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let team = units.register(NewUnit {
            kind: UnitKind::Orchestration {
                orchestration_id: "orch-1".into(),
                name: "team".into(),
                terminal_pane_id: "o-0".into(),
                terminal_agent_id: "10".into(),
            },
            ..single("team", "a", "unused", "unused")
        });
        let one = units.register(single("fix", "a", "p1", "1"));
        assert_eq!(units.note_generation_replaced("other", "10", "11"), None);
        assert_eq!(
            units.note_generation_replaced("o-0", "10", "11"),
            Some(team.clone())
        );
        assert_eq!(units.mark_completed("o-0", "10", 5), None);
        assert_eq!(units.mark_completed("o-0", "11", 5), Some(team));
        assert_eq!(
            units.note_generation_replaced("p1", "1", "2"),
            Some(one.clone())
        );
        assert_eq!(
            units
                .unit_of_generation(Some("p1"), "2", None)
                .map(|u| &u.id),
            Some(&one)
        );
    }

    #[test]
    fn stale_ids_from_a_previous_daemon_are_unknown_with_a_restart_hint() {
        let mut before = DispatchedUnits::with_epoch("aaaaaa");
        let id = before.register(single("fix", "a", "p1", "1"));
        let mut after = DispatchedUnits::with_epoch("bbbbbb");
        after.register(single("fix", "a", "p1", "1"));
        let refused = after.resolve_id(&id).expect_err("foreign id");
        assert_eq!(refused.reason, RefusalReason::UnknownUnit);
        assert!(refused.message.contains("restarted"), "{}", refused.message);
    }

    #[test]
    fn authorize_allows_the_dispatcher_and_refuses_everyone_else() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let id = units.register(single("fix", "a", "p1", "1"));
        let unit = units.get(&id).unwrap();
        assert!(authorize(&agent("a"), unit).is_ok());
        assert!(authorize(&Caller::Person, unit).is_ok());
        // A sibling dispatcher.
        assert_eq!(
            authorize(&agent("b"), unit).unwrap_err().reason,
            RefusalReason::NotYourUnit
        );
        // A NEW occupant of the dispatcher's own pane: same pane, new agent id.
        let new_occupant = Caller::Agent {
            pane_id: "a-pane".to_string(),
            agent_id: "a2".to_string(),
            orchestration_id: None,
        };
        assert_eq!(
            authorize(&new_occupant, unit).unwrap_err().reason,
            RefusalReason::NotYourUnit
        );
    }

    #[test]
    fn authorize_refuses_the_callers_own_pane_and_own_instance() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        // A unit whose pane is the caller's own (a dispatcher that dispatched
        // into... itself can't happen, but a recycled record can say so).
        let id = units.register(single("self", "a", "a-pane", "1"));
        assert_eq!(
            authorize(&agent("a"), units.get(&id).unwrap())
                .unwrap_err()
                .reason,
            RefusalReason::OwnPane
        );
        let orch = units.register(orchestration("team", "a", "orch-1", "p9", "9"));
        let in_team = Caller::Agent {
            pane_id: "p10".to_string(),
            agent_id: "a".to_string(),
            orchestration_id: Some("orch-1".to_string()),
        };
        assert_eq!(
            authorize(&in_team, units.get(&orch).unwrap())
                .unwrap_err()
                .reason,
            RefusalReason::OwnPane
        );
    }

    #[test]
    fn closing_is_not_transitive_and_descendants_are_listed() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let child = units.register(single("child", "a", "p1", "1"));
        // The child's own agent ("1") dispatched a grandchild.
        let grandchild = units.register(single("grandchild", "1", "p2", "2"));
        // The top dispatcher may close the child, not the grandchild.
        assert!(authorize(&agent("a"), units.get(&child).unwrap()).is_ok());
        assert_eq!(
            authorize(&agent("a"), units.get(&grandchild).unwrap())
                .unwrap_err()
                .reason,
            RefusalReason::NotYourUnit
        );
        let agents = vec!["1".to_string()];
        let open: Vec<_> = units
            .open_descendants(&agents)
            .map(|u| u.id.clone())
            .collect();
        assert_eq!(open, vec![grandchild]);
    }

    #[test]
    fn completion_is_marked_only_by_the_authorized_terminal_generation() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let id = units.register(single("fix", "a", "p1", "7"));
        // A late report from a predecessor generation in the same pane.
        assert_eq!(units.mark_completed("p1", "6", 10), None);
        // A successor generation in the same pane.
        assert_eq!(units.mark_completed("p1", "8", 10), None);
        assert_eq!(units.get(&id).unwrap().completed_at_ms, None);
        // The authorized generation; duplicates keep the first time.
        assert_eq!(units.mark_completed("p1", "7", 10), Some(id.clone()));
        assert_eq!(units.mark_completed("p1", "7", 20), Some(id.clone()));
        assert_eq!(units.get(&id).unwrap().completed_at_ms, Some(10));
    }

    #[test]
    fn a_worker_role_cannot_mark_the_orchestration_complete() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let id = units.register(orchestration("team", "a", "orch-1", "p-orch", "10"));
        assert_eq!(units.mark_completed("p-worker", "11", 1), None);
        assert_eq!(units.get(&id).unwrap().completed_at_ms, None);
        assert_eq!(units.mark_completed("p-orch", "10", 1), Some(id));
    }

    #[test]
    fn lifecycle_exited_closing_ended() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let id = units.register(single("fix", "a", "p1", "1"));
        units.note_exited(&id);
        assert_eq!(units.get(&id).unwrap().state, UnitState::Exited);
        // Exited is still closeable by name.
        assert!(units.resolve_name("fix", &Caller::Person).is_ok());
        let prior = units.begin_closing(&id).expect("begins");
        assert_eq!(prior, UnitState::Exited);
        assert!(units.begin_closing(&id).is_none(), "already closing");
        // An EOF while closing does not change it.
        units.note_exited(&id);
        assert_eq!(units.get(&id).unwrap().state, UnitState::Closing);
        units.abort_closing(&id, prior);
        assert_eq!(units.get(&id).unwrap().state, UnitState::Exited);
        units.begin_closing(&id);
        let stone = units.end(&id, EndReason::ClosedByVerb, 3).expect("ended");
        assert_eq!(stone.kind, "single");
        assert!(units.get(&id).is_none());
        assert_eq!(units.live_len(), 0);
    }

    #[test]
    fn an_orchestration_unit_is_found_through_any_member_generation() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let id = units.register(orchestration("team", "a", "orch-1", "p-orch", "10"));
        // The orchestrator closed first: a worker generation still finds it.
        let found = units
            .unit_of_generation(Some("p-worker"), "11", Some("orch-1"))
            .expect("a worker of the instance");
        assert_eq!(found.id, id);
        assert!(units.unit_of_generation(Some("p-x"), "12", None).is_none());
        assert_eq!(units.unit_of_instance("orch-1").unwrap().id, id);
    }

    #[test]
    fn tombstones_are_bounded_and_text_capped() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let long = "x".repeat(MAX_RECORDED_TEXT_CHARS * 3);
        for i in 0..(MAX_TOMBSTONES + 10) {
            let id = units.register(single(&format!("{long}{i}"), "a", "p", &i.to_string()));
            units.end(&id, EndReason::Stopped, i as i64);
        }
        assert_eq!(units.tombstones().count(), MAX_TOMBSTONES);
        assert!(
            units
                .tombstones()
                .all(|t| t.slug.chars().count() <= MAX_RECORDED_TEXT_CHARS)
        );
    }

    #[test]
    fn the_retained_name_is_bounded() {
        let mut units = DispatchedUnits::with_epoch("aaaaaa");
        let id = units.register(single(&"n".repeat(1000), "a", "p", "1"));
        assert_eq!(
            units.get(&id).unwrap().name.chars().count(),
            MAX_RECORDED_TEXT_CHARS
        );
    }

    #[test]
    fn every_session_status_against_the_busy_set() {
        let table = [
            (SessionStatus::Thinking, true),
            (SessionStatus::Working, true),
            (SessionStatus::Compacting, true),
            (SessionStatus::WaitingForInput, true),
            (SessionStatus::Blocked, true),
            (SessionStatus::Idle, false),
            (SessionStatus::Error, false),
            (SessionStatus::Unknown, false),
        ];
        for (status, busy) in table {
            assert_eq!(is_busy(&status), busy, "{status:?}");
        }
    }

    #[test]
    fn the_refusal_is_the_same_whatever_selector_reached_the_target() {
        // The policy reads only the resolved target's facts, so a unit reached
        // by name, by `--pane` or by `--orchestration-of` gets one answer.
        let facts = TargetFacts {
            unit_reported: Some(false),
            statuses: vec![SessionStatus::Idle],
            strands_orchestration: false,
        };
        let by_name = default_refusals(&facts);
        let by_pane = default_refusals(&facts.clone());
        assert_eq!(by_name, by_pane);
        assert_eq!(by_name[0].0, RefusalReason::NotReported);
        // Busy and stranding stack after it.
        let facts = TargetFacts {
            unit_reported: Some(true),
            statuses: vec![SessionStatus::Idle, SessionStatus::Working],
            strands_orchestration: true,
        };
        let reasons: Vec<_> = default_refusals(&facts).into_iter().map(|r| r.0).collect();
        assert_eq!(
            reasons,
            vec![RefusalReason::Busy, RefusalReason::StrandsOrchestration]
        );
        // An ordinary pane has no report to check.
        assert!(default_refusals(&TargetFacts::default()).is_empty());
    }

    #[test]
    fn only_the_three_target_state_refusals_are_forceable() {
        use RefusalReason::*;
        for r in [NotReported, Busy, StrandsOrchestration] {
            assert!(r.forceable(), "{r:?}");
        }
        for r in [
            NotAttested,
            Superseded,
            NotYourUnit,
            OwnPane,
            UnknownUnit,
            Ambiguous,
            AlreadyEnded,
            BulkRequiresDryRun,
            SelectorTooLarge,
        ] {
            assert!(!r.forceable(), "{r:?}");
        }
    }

    #[test]
    fn reasons_are_kebab_case_on_the_wire_and_unknown_decodes() {
        for r in [
            RefusalReason::NotReported,
            RefusalReason::StrandsOrchestration,
            RefusalReason::BulkRequiresDryRun,
            RefusalReason::NotYourUnit,
            RefusalReason::SelectorTooLarge,
        ] {
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                serde_json::Value::String(r.code().to_string())
            );
        }
        let decoded: RefusalReason = serde_json::from_str("\"from-the-future\"").unwrap();
        assert_eq!(decoded, RefusalReason::Unknown);
    }
}
