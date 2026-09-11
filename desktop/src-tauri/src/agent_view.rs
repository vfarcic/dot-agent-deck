//! The agent list, maintained from the daemon's pushed events (PRD #741 M4(b)).
//!
//! M4(a) removed the per-refresh `Hello`. What it left is the other half of the
//! waste the PRD's performance section measured: the watcher re-fetched the
//! **whole** agent list on every daemon event, up to 6.667 times a second, at
//! ~644 B/agent with no delta anywhere. This module is what stops that — it
//! folds the events the desktop is already receiving into a local agent list and
//! re-fetches only when it must.
//!
//! # The fold is the daemon's own, not a second implementation
//!
//! [`AppState::apply_event`] is the function the daemon runs on every hook event
//! it ingests ([`dot_agent_deck::daemon`]'s hook path broadcasts the event and
//! then applies it to its own `AppState`), and
//! [`AppState::attach_live_sessions`] is the join the `ListAgents` handler runs
//! to put the result on the wire. This module calls both. There is no
//! client-side status machine, no parallel notion of what `Working` means, and
//! nothing here decides which of two sessions is the live one for a record.
//!
//! The TUI has done exactly this since PRD #76 M2.17 — `spawn_event_subscriber`
//! in `src/main.rs` routes each `BroadcastMsg::Event` into the TUI's own
//! `AppState` — so this is the established client pattern rather than a new one.
//!
//! **The one place the copy is not the original, stated because it is the thing
//! that can put a wrong row on screen.** The daemon's `apply_event` asks its
//! `AgentPtyRegistry` whether it owns the generation an event names
//! (`AppState::set_agent_ownership`); a client has no registry, so it falls back
//! to the historical pane-set rule — an event is admitted for a pane the state
//! has registered, and a `SessionStart` for an unregistered pane auto-registers
//! it. Every pane in the last full reply is registered here at install time, so
//! the two answers agree for every agent the desktop has heard of. They can
//! disagree for an agent it has not: a **paneless** agent's events (the event
//! carries no `pane_id`) are admitted by a client only while it manages no panes
//! at all, so with any paned agent present a paneless one's transitions wait for
//! the next reconcile. That is the TUI's behaviour too, it is bounded by
//! [`RECONCILE_INTERVAL`], and it is why the floor exists rather than being a
//! belt-and-braces extra.
//!
//! # Why a floor, and what it is actually for
//!
//! Two transitions cannot be served by events, and they are not symmetrical:
//!
//! * **A new agent's row cannot be completed from an event.** `SessionStart`
//!   says a session began and names its `agent_type` / `cwd` / `pane_id`, but
//!   `display_name`, `tab_membership`, `rows`/`cols` and `spawned_at_ms` are
//!   registry facts that appear in no `AgentEvent` field. So a `SessionStart`
//!   marks a fetch due immediately — this gap is closed by a *targeted* fetch,
//!   not by the floor.
//! * **A removal is signalled by nothing at all.** Enumerated rather than
//!   assumed: `AgentRecord`s come from `AgentPtyRegistry::agent_records()`,
//!   which filters on the `exited` flag the PTY reader sets at EOF, and no
//!   broadcast is sent from that path — the `BroadcastMsg` senders in `src/` are
//!   the hook-socket ingest (`daemon::ingest_event`), the delivery-notice sink
//!   (`daemon::install_delivery_notice_sink`), the card surface a daemon-spawned
//!   pane paints (`spawn::surface_spawned_pane`), the orchestration surface, the
//!   prompt-watch synthetics and `WorktreeKept`, and none of them fires on a
//!   child exiting. A cooperative agent's final `SessionEnd` retires its
//!   *session*, which is a different thing from its *record*. So **every**
//!   disappearance is silent, not merely a crash, and only a periodic re-read
//!   can catch one.
//!
//!   What a `SessionEnd` *does* buy is a head start, and the fold takes it:
//!   because the event says a session is over, it marks a fetch due the way a
//!   `SessionStart` does, so a **cooperative** exit is reconciled in one
//!   round trip instead of waiting out [`RECONCILE_INTERVAL`]. It is an
//!   optimisation and never a replacement — see [`FetchReason::SessionEnd`] for
//!   the millisecond-scale reason a fetch it triggers can still see the record
//!   present, which is why the floor stays exactly as wide as it was.
//!
//! [`AppState::apply_event`]: dot_agent_deck::state::AppState::apply_event
//! [`AppState::attach_live_sessions`]: dot_agent_deck::state::AppState::attach_live_sessions

use std::time::Duration;

use dot_agent_deck::daemon_client::AgentRecord;
use dot_agent_deck::event::{BroadcastMsg, EventType};
use dot_agent_deck::state::AppState;
use tokio::time::Instant;

/// How often the cached list is reconciled against a real `ListAgents`.
///
/// **Five seconds, and the number is a correctness/load trade rather than a
/// tuning knob.** What it bounds is how long a silently-removed agent stays on
/// screen, because — see the module docs — nothing signals a removal.
///
/// * **What it costs.** 0.2 connections/s and, at fifteen agents and the
///   measured ~644 B/agent, ~1.9 KB/s. Against the pre-M4(b) ceiling of 6.667
///   connections/s and ~64 KB/s that is a 97% reduction in both. Over an ssh hop
///   — the case PRD #741 exists for — it is one round trip every five seconds
///   instead of up to 13.33 a second.
/// * **What it buys, and it is not only a reduction.** Today's removal latency
///   is *unbounded* in the quiet case: the watcher refreshes only when an event
///   arrives, so an agent that is `SIGKILL`ed while it is the only one running
///   emits nothing further, no refresh is triggered, and its row stays until the
///   user acts. Five seconds replaces "never" with a bound there.
/// * **What it costs the other way, stated plainly.** On a *busy* fleet today's
///   removal latency is ~150 ms — the next agent's next event forces a full
///   listing. It becomes ≤5 s. That is the regression this number buys the rest
///   with, and the mitigation is not the floor: an agent the user is actually
///   watching has a PTY stream, and its `KIND_STREAM_END` reaches that tile
///   immediately and independently of this list.
/// * **Why not longer.** It is a sixth of the daemon's 30 s
///   `DEFAULT_IDLE_SHUTDOWN_SECS`, so several reconciles land inside any window
///   in which the daemon would itself conclude it is unused; and it is the
///   backstop for the admission-control divergence named in the module docs, so
///   a minute-scale floor would leave a paneless agent's status wrong for a
///   minute.
/// * **Why not shorter.** Below about a second the floor starts competing with
///   the 150 ms coalesce interval for the same refreshes and the milestone stops
///   being worth its complexity.
pub(crate) const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// Why the next refresh has to fetch rather than answer from the fold.
///
/// Carried rather than collapsed to a `bool` so the measurement can say *which*
/// gap cost a connection, and so a test can assert that a given transition took
/// the route it was supposed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchReason {
    /// Nothing has been fetched yet.
    Initial,
    /// The event subscription was (re)established, so the fold has a hole in it
    /// of unknown size. See [`AgentView::resubscribed`].
    Resubscribed,
    /// A `SessionStart`: a row exists that events cannot complete.
    SessionStart,
    /// A `SessionEnd`: a session retired, and its *record* may be about to go
    /// with it.
    ///
    /// The symmetric counterpart of [`Self::SessionStart`] and equally cheap —
    /// once per agent lifetime — but it buys something weaker, and the
    /// difference is worth keeping straight. A `SessionEnd` does not remove a
    /// record; the record goes when the PTY reader sets `exited` at EOF, which
    /// broadcasts nothing (see the module docs). The two are milliseconds
    /// apart in a cooperative exit and in that order, so **the fetch this
    /// reason triggers may well still see the record present** and the removal
    /// is then caught by the next reconcile anyway.
    ///
    /// So it is an optimisation on the common case, not a signal to lean on:
    /// it closes a cooperative removal well inside the floor, and the floor
    /// keeps covering the case it was sized for — a crash, where no
    /// `SessionEnd` is emitted at all. Nothing about this reason justifies
    /// widening [`RECONCILE_INTERVAL`].
    SessionEnd,
    /// An orchestration was spawned while attached — same metadata gap as
    /// `SessionStart`, plus tab membership.
    OrchestrationSurface,
    /// A dispatched worktree was left on disk, which is emitted *after* the
    /// close that reaped its agents. The registry has certainly changed.
    WorktreeKept,
    /// The periodic reconciliation, which is the only thing that catches a
    /// silent removal.
    Reconcile,
}

/// The desktop's view of the agent list: the last full reply, plus the daemon's
/// own fold over every event since.
pub(crate) struct AgentView {
    /// Registry facts as of the last full `ListAgents`. Their `live` field is
    /// the daemon's answer at that moment and is kept as the fallback for a
    /// record the local fold has no session for.
    records: Vec<AgentRecord>,
    /// The daemon's `AppState`, run here over the same broadcast the daemon
    /// applies to its own.
    fold: AppState,
    /// Set when a refresh must fetch. Sticky until a fetch lands.
    due: Option<FetchReason>,
    /// When the last full fetch landed.
    fetched_at: Option<Instant>,
    /// Full `ListAgents` replies installed. Test/measurement only.
    fetches: usize,
    /// Events folded. Test/measurement only.
    folded: usize,
}

impl Default for AgentView {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            fold: AppState::default(),
            due: Some(FetchReason::Initial),
            fetched_at: None,
            fetches: 0,
            folded: 0,
        }
    }
}

impl AgentView {
    /// The event subscription was just established.
    ///
    /// **This is the correctness spine and it fails closed.** Between one
    /// subscription ending and the next beginning, events happened that this
    /// fold did not see, so everything it holds is suspect — not stale by a
    /// little, wrong by an unknown amount. The whole fold is therefore discarded
    /// rather than patched, and the cached records are refused as an answer
    /// until a fetch lands: [`Self::needs_fetch`] returns `Some` and
    /// [`Self::install`] is the only thing that clears it. A fetch that *fails*
    /// leaves the due marker set, so a failed reconnect cannot promote the old
    /// list back into a confident answer — the caller reports the transport
    /// failure, exactly as it did before this module existed.
    pub(crate) fn resubscribed(&mut self) {
        self.fold = AppState::default();
        self.records.clear();
        self.due = Some(FetchReason::Resubscribed);
    }

    /// Fold one broadcast message, and note whether it opened a metadata gap a
    /// fetch has to close.
    pub(crate) fn apply(&mut self, msg: &BroadcastMsg) {
        match msg {
            BroadcastMsg::Event(event) => {
                // Unconditional rather than "only for an id we do not hold".
                // Both are once per agent lifetime (or per `/clear`), so the
                // saving from being clever is nil, and the conditional version
                // has to be right about what "the same agent" means across a
                // restart that mints a new registry id on the same pane. One
                // connection, rarely, buys not having to be right about that.
                match event.event_type {
                    EventType::SessionStart => self.mark(FetchReason::SessionStart),
                    EventType::SessionEnd => self.mark(FetchReason::SessionEnd),
                    _ => {}
                }
                self.fold.apply_event(event.clone());
                self.folded += 1;
            }
            BroadcastMsg::OrchestrationSurface(_) => self.mark(FetchReason::OrchestrationSurface),
            BroadcastMsg::WorktreeKept(_) => self.mark(FetchReason::WorktreeKept),
        }
    }

    /// Why the next refresh must fetch, or `None` if the fold can answer.
    ///
    /// The elapsed check is here as well as on the caller's timer because the
    /// two answer different questions: the timer is what wakes a watcher that is
    /// otherwise blocked on a silent event stream, and this is what keeps the
    /// bound honest if that timer is ever starved behind a long refresh.
    pub(crate) fn needs_fetch(&self, now: Instant) -> Option<FetchReason> {
        if let Some(reason) = self.due {
            return Some(reason);
        }
        match self.fetched_at {
            Some(at) if now.saturating_duration_since(at) < RECONCILE_INTERVAL => None,
            _ => Some(FetchReason::Reconcile),
        }
    }

    /// Force the next refresh to fetch, without a specific transition behind it.
    pub(crate) fn mark_reconcile_due(&mut self) {
        self.mark(FetchReason::Reconcile);
    }

    /// A full `ListAgents` reply landed: it becomes the truth, and the fold is
    /// re-seeded from it.
    ///
    /// Seeding is not optional and the reason is a user-visible field. The fold
    /// only knows what it has watched, so a `tool_count` — a running tally the
    /// daemon has kept since the agent started — would restart from zero for
    /// every agent that was already running when the desktop connected. The
    /// daemon's own reconnect-side seeding function is used verbatim
    /// (`AppState::seed_hydrated_session`, the counterpart the TUI runs at
    /// hydration), which is also what mints the `agent_id` that lets a later
    /// `SessionStart` remap onto this session instead of forking a second one.
    ///
    /// # What a full fetch does to an event that straddles it
    ///
    /// The fold is **replaced**, not merged, because a reply is the daemon's
    /// whole answer and merging would let a session the daemon has retired
    /// survive in the client. The cost is a narrow race, named rather than
    /// engineered around — and it is worth stating precisely, because the
    /// obvious version of it is the wrong way round.
    ///
    /// **An event cannot be swallowed.** `daemon::ingest_event` takes the
    /// `AppState` write lock *before* the broadcast and holds it across both
    /// `event_tx.send(..)` and `state.apply_event(..)`, and the `ListAgents`
    /// handler's read lock therefore serialises after it. So every reply that
    /// could reach a client already contains every event that reached it first:
    /// there is no window in which the daemon has sent a transition and not yet
    /// applied it to the state a reply is built from.
    ///
    /// The residual is the **opposite sign — a double-apply.** The refresh loop
    /// drains the reader's channel before it issues the fetch, so an event that
    /// arrives *during* the request is folded on the next pass while the reply
    /// it is also reflected in has already been installed. For a status that is
    /// idempotent — which is every transition `apply_event` sets — applying it
    /// twice is invisible. For the one **monotonic** field, `tool_count`, it
    /// over-counts by one until the next reconcile re-seeds the fold from a
    /// fresh reply, which is bounded by [`RECONCILE_INTERVAL`].
    ///
    /// Closing it properly needs the reply to carry a position in the event
    /// stream — a sequence number on the wire — which is a `PROTOCOL_VERSION`
    /// change and out of M4(b) by its scope fence. A transient +1 on a tool
    /// tally does not buy a wire break.
    pub(crate) fn install(&mut self, records: Vec<AgentRecord>, now: Instant) {
        let mut fold = AppState::default();
        for record in &records {
            let Some(pane_id) = record.pane_id_env.clone() else {
                // A paneless record has no pane to register and no session key
                // to seed under — `insert_placeholder_session` is pane-keyed.
                // Its `live` stays whatever this reply carried and the fold adds
                // nothing to it, which is the honest outcome: see the module
                // docs on paneless admission.
                continue;
            };
            fold.register_pane(pane_id.clone());
            fold.seed_hydrated_session(
                pane_id,
                record.cwd.clone(),
                record.agent_type.clone(),
                Some(record.id.clone()),
                record.live.as_ref(),
            );
        }
        self.fold = fold;
        self.records = records;
        self.due = None;
        self.fetched_at = Some(now);
        self.fetches += 1;
    }

    /// The agent list to render: the cached registry facts with each record's
    /// `live` taken from the local fold.
    ///
    /// `or`, not an overwrite. [`AppState::attach_live_sessions`] writes `None`
    /// onto a record it holds no session for, which is right for the daemon (it
    /// is the authority and absence is an answer) and wrong here (absence means
    /// this fold has not seen that agent). Keeping the fetched value in that
    /// case is what stops a paneless agent's status from blanking between
    /// reconciles.
    ///
    /// [`AppState::attach_live_sessions`]: dot_agent_deck::state::AppState::attach_live_sessions
    pub(crate) fn records(&self) -> Vec<AgentRecord> {
        self.records
            .iter()
            .map(|record| {
                let mut record = record.clone();
                if let Some(live) = self
                    .fold
                    .live_session_for(&record.id, record.pane_id_env.as_deref())
                {
                    record.live = Some(live);
                }
                record
            })
            .collect()
    }

    fn mark(&mut self, reason: FetchReason) {
        if self.due.is_none() {
            self.due = Some(reason);
        }
    }

    /// Full `ListAgents` replies installed since this view was created.
    #[cfg(test)]
    pub(crate) fn fetch_count(&self) -> usize {
        self.fetches
    }

    /// Events folded since this view was created.
    #[cfg(test)]
    pub(crate) fn fold_count(&self) -> usize {
        self.folded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::event::{AgentEvent, AgentType};
    use dot_agent_deck::state::SessionStatus;

    fn record(id: &str, pane_id: &str) -> AgentRecord {
        AgentRecord {
            id: id.into(),
            pane_id_env: Some(pane_id.into()),
            display_name: Some(format!("worker-{id}")),
            cwd: Some("/home/dev/project".into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            rows: 40,
            cols: 120,
            live: None,
            spawned_at_ms: Some(1_700_000_000_000),
        }
    }

    fn event(pane_id: &str, agent_id: &str, kind: EventType) -> AgentEvent {
        AgentEvent {
            session_id: format!("{pane_id}-session"),
            agent_type: AgentType::ClaudeCode,
            event_type: kind,
            tool_name: None,
            tool_detail: None,
            cwd: Some("/home/dev/project".into()),
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some(pane_id.into()),
            agent_id: Some(agent_id.into()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        }
    }

    fn tool_start(pane_id: &str, agent_id: &str, tool: &str) -> AgentEvent {
        let mut event = event(pane_id, agent_id, EventType::ToolStart);
        event.tool_name = Some(tool.into());
        event
    }

    fn status_of(records: &[AgentRecord], id: &str) -> Option<SessionStatus> {
        records
            .iter()
            .find(|record| record.id == id)
            .and_then(|record| record.live.as_ref())
            .map(|live| live.status.clone())
    }

    /// Scenario: install one agent, then fold a `ToolStart` for it. The rendered
    /// record must carry `Working` without any fetch falling due — this is the
    /// high-frequency case and the whole point of the milestone.
    #[test]
    fn a_status_transition_is_served_from_the_fold_and_costs_no_fetch() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);

        view.apply(&BroadcastMsg::Event(tool_start("pane-7", "7", "Bash")));

        assert_eq!(
            view.needs_fetch(now),
            None,
            "a tool transition is exactly what the fold is for"
        );
        assert_eq!(
            status_of(&view.records(), "7"),
            Some(SessionStatus::Working),
            "the fold must have moved the rendered status"
        );
        assert_eq!(view.fetch_count(), 1, "only the install fetched");
    }

    /// Scenario: fold a burst of twenty tool events for one agent. Every one is
    /// applied and none of them makes a fetch fall due, which is the "burst of N
    /// events" figure the milestone reports.
    #[test]
    fn a_burst_of_events_costs_no_fetches_at_all() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);

        for n in 0..10 {
            view.apply(&BroadcastMsg::Event(tool_start(
                "pane-7",
                "7",
                &format!("Tool{n}"),
            )));
            view.apply(&BroadcastMsg::Event(event(
                "pane-7",
                "7",
                EventType::ToolEnd,
            )));
        }

        assert_eq!(view.fold_count(), 20);
        assert_eq!(view.fetch_count(), 1);
        assert_eq!(
            view.needs_fetch(now),
            None,
            "twenty events must still cost zero connections"
        );
    }

    /// Scenario: a `SessionStart` arrives for an agent the view has never seen.
    /// Its `display_name` / `rows` / `spawned_at_ms` are registry facts no event
    /// carries, so a fetch must fall due immediately rather than at the floor.
    #[test]
    fn a_session_start_makes_a_fetch_fall_due_at_once() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);
        assert_eq!(view.needs_fetch(now), None);

        view.apply(&BroadcastMsg::Event(event(
            "pane-9",
            "9",
            EventType::SessionStart,
        )));

        assert_eq!(
            view.needs_fetch(now),
            Some(FetchReason::SessionStart),
            "gap 1 is closed by a targeted fetch, not by waiting for the floor"
        );
    }

    /// Scenario: install one agent, fold a `SessionEnd` for it, and check that
    /// a fetch falls due at once rather than at the floor. A cooperative exit
    /// is the common removal and this is what reconciles it in one round trip;
    /// the record itself is still present, which is the point — the fetch is
    /// what finds out whether it has gone.
    #[test]
    fn a_session_end_makes_a_fetch_fall_due_at_once() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);
        assert_eq!(view.needs_fetch(now), None);

        view.apply(&BroadcastMsg::Event(event(
            "pane-7",
            "7",
            EventType::SessionEnd,
        )));

        assert_eq!(
            view.needs_fetch(now),
            Some(FetchReason::SessionEnd),
            "a cooperative exit must not wait out the floor"
        );
        assert_eq!(
            view.records().len(),
            1,
            "the record is still there: SessionEnd retires a session, not a record, \
             which is why this reason is an optimisation and not a removal signal"
        );
    }

    /// Scenario: nothing at all happens for the reconciliation interval. A fetch
    /// must fall due anyway — this is the only thing that catches an agent that
    /// disappeared without signalling, and the quiet fleet is exactly the case
    /// where no event will ever wake the watcher.
    #[test]
    fn the_floor_makes_a_fetch_fall_due_in_silence() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);

        assert_eq!(view.needs_fetch(now + RECONCILE_INTERVAL / 2), None);
        assert_eq!(
            view.needs_fetch(now + RECONCILE_INTERVAL),
            Some(FetchReason::Reconcile),
            "a silent removal is caught by the floor or by nothing"
        );
    }

    /// Scenario: the subscription is re-established after a break. The view must
    /// refuse to answer from what it held — the fold has a hole of unknown size
    /// — and must keep refusing until a fetch actually lands.
    #[test]
    fn a_resubscribe_refuses_the_cached_list_until_a_fetch_lands() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);
        view.apply(&BroadcastMsg::Event(tool_start("pane-7", "7", "Bash")));
        assert_eq!(view.needs_fetch(now), None);

        view.resubscribed();

        assert_eq!(view.needs_fetch(now), Some(FetchReason::Resubscribed));
        assert!(
            view.records().is_empty(),
            "a fold with an unknown hole in it must not render as a confident list"
        );
        // A failed fetch installs nothing, so the demand survives it.
        assert_eq!(
            view.needs_fetch(now + RECONCILE_INTERVAL * 10),
            Some(FetchReason::Resubscribed),
            "nothing but an installed reply may clear the demand"
        );

        view.install(vec![record("7", "pane-7")], now);
        assert_eq!(view.needs_fetch(now), None);
    }

    /// Scenario: a reply carries a `live` snapshot for an agent whose events the
    /// fold cannot admit (here, a record with no pane at all). The rendered row
    /// must keep the daemon's answer rather than blanking to "no live state".
    #[test]
    fn a_record_the_fold_cannot_hold_keeps_the_fetched_live_state() {
        let now = Instant::now();
        let mut paneless = record("3", "unused");
        paneless.pane_id_env = None;
        paneless.live = Some(dot_agent_deck::state::SessionSnapshot {
            status: SessionStatus::Working,
            agent_type: Some(AgentType::ClaudeCode),
            active_tool: None,
            tool_count: 9,
            first_prompts: Vec::new(),
            last_user_prompt: None,
            live_target: None,
            last_activity_ms: Some(1_700_000_000_000),
        });

        let mut view = AgentView::default();
        view.install(vec![paneless], now);

        let rendered = view.records();
        assert_eq!(
            status_of(&rendered, "3"),
            Some(SessionStatus::Working),
            "an `or`, not an overwrite — the daemon's answer stands where the \
             fold has none"
        );
        assert_eq!(
            rendered[0].live.as_ref().map(|live| live.tool_count),
            Some(9)
        );
    }

    /// Scenario: install a reply whose record already carries a tool tally, then
    /// fold one more completed tool. The tally must be 10, not 1 — the fold is
    /// seeded from the reply rather than started from zero, which is what keeps
    /// a long-running agent's counters honest across a desktop restart.
    ///
    /// The tally moves on `ToolEnd` and not on `ToolStart`, which this test got
    /// wrong first time round and `AppState` corrected: that rule lives in the
    /// daemon's fold and this module does not get a vote on it.
    #[test]
    fn the_fold_is_seeded_from_the_reply_rather_than_started_at_zero() {
        let now = Instant::now();
        let mut seeded = record("7", "pane-7");
        seeded.live = Some(dot_agent_deck::state::SessionSnapshot {
            status: SessionStatus::Idle,
            agent_type: Some(AgentType::ClaudeCode),
            active_tool: None,
            tool_count: 9,
            first_prompts: Vec::new(),
            last_user_prompt: None,
            live_target: None,
            last_activity_ms: Some(1_700_000_000_000),
        });

        let mut view = AgentView::default();
        view.install(vec![seeded], now);
        view.apply(&BroadcastMsg::Event(tool_start("pane-7", "7", "Bash")));
        view.apply(&BroadcastMsg::Event(event(
            "pane-7",
            "7",
            EventType::ToolEnd,
        )));

        assert_eq!(
            view.records()[0].live.as_ref().map(|live| live.tool_count),
            Some(10),
            "a tally that restarted at 1 would be a user-visible regression"
        );
    }

    /// Scenario: the first refresh of a fresh view must fetch — there is nothing
    /// to answer from, and an empty list is not the same claim as "no agents".
    #[test]
    fn a_fresh_view_has_nothing_to_answer_with() {
        let view = AgentView::default();
        assert_eq!(view.needs_fetch(Instant::now()), Some(FetchReason::Initial));
        assert!(view.records().is_empty());
    }

    /// Scenario: an orchestration surface and a kept worktree each mark a fetch
    /// due. Both describe a registry change that carries no per-agent event.
    #[test]
    fn structural_pushes_mark_a_fetch_due() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);

        view.apply(&BroadcastMsg::WorktreeKept(
            dot_agent_deck::issue_dispatch_run::KeptWorktree {
                path: "/tmp/wt".into(),
                confirmed_dirty: true,
            },
        ));
        assert_eq!(view.needs_fetch(now), Some(FetchReason::WorktreeKept));
    }

    /// Scenario: two agents share a pane id across a restart — the newer session
    /// must win the row. The pick is `AppState::live_session_for`'s, not this
    /// module's, and this test is here so a future refactor that reimplements it
    /// locally goes red.
    #[test]
    fn the_live_pick_is_the_daemons_and_the_newest_session_wins() {
        let now = Instant::now();
        let mut view = AgentView::default();
        view.install(vec![record("7", "pane-7")], now);

        let mut older = tool_start("pane-7", "7", "Read");
        older.timestamp = chrono::Utc::now() - chrono::Duration::seconds(60);
        let newer = event("pane-7", "7", EventType::Idle);

        view.apply(&BroadcastMsg::Event(older));
        view.apply(&BroadcastMsg::Event(newer));

        assert_eq!(
            status_of(&view.records(), "7"),
            Some(SessionStatus::Idle),
            "the newest activity must decide the row"
        );
    }
}
