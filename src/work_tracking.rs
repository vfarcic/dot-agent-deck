//! PRD #1889: the daemon's side of the work journal. [`crate::work_journal`]
//! is the file format and the store; this module decides **which work** each
//! launch, attempt, report and end belongs to, and writes the event.
//!
//! The registry owns one [`WorkTracker`]. It holds the run's
//! [`WorkJournal`] (installed at startup by `run_daemon_with`; a registry that
//! never had one installed, such as a unit test's, records nothing and mints
//! no work ids) and an in-memory book of the works this run knows:
//!
//! - the work of each orchestration instance, so every role pane of it is an
//!   attempt of the same work and a delegation can name it as its parent;
//! - the works minted for a pane at its spawn, so closing the pane closes them;
//! - the work each live attempt (agent generation) serves, so its end is
//!   written against it after its record is gone;
//! - each delegated task's work and each dispatched unit's work, so a report
//!   and an end find them.
//!
//! Every entry in the book is retired on a terminal path of what it tracks,
//! so the book is bounded by what is live, not by how long the daemon runs:
//! a refused or failed spawn rolls back what it minted ([`SpawnWork`]), a
//! delegated task is retired on its report, its supersession, the end of its
//! worker generation and its orchestration's end, and a scheduled fire that
//! re-posts into an existing pane closes at once ([`WorkTracker::fire_settled`]).
//!
//! No method here does file I/O under the book's mutex: the journal's
//! [`WorkJournal::record`] only queues the event for the journal's writer
//! thread, and the filesystem reads a workspace needs are made before the
//! mutex is taken. The book's mutex is a leaf; call these with no registry,
//! hub or other ranked lock held — except [`WorkTracker::attempt_started`],
//! which the spawn calls under the registry lock that publishes the
//! generation, so a stop of it is ordered after its start.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use crate::agent_pty::{TabMembership, Termination};
use crate::event::AgentType;
use crate::work_journal::{
    CloseHow, Destination, EndCause, EndHow, Event, Launch, Refs, WorkId, WorkJournal,
};

/// Ends that arrived before their attempt was started (a child that exits
/// before the spawn path wrote its start). Bounded: an end whose attempt
/// never starts (a spawn refused after its fork) is otherwise never taken.
const MAX_EARLY_ENDS: usize = 64;

/// Delegated tasks retired without a report (superseded, their worker gone,
/// their orchestration ended), kept so a report that is already on its way
/// still finds its work. Bounded: the oldest settled one goes first. A task
/// whose post has not come back yet (its events still held) is never evicted:
/// its post's callback ([`WorkTracker::delegate_posted`] or
/// [`WorkTracker::delegate_refused`]) is still to come and takes it, so those
/// are bounded by the posts in flight, not by this cap.
const MAX_RETIRED_TASKS: usize = 64;

/// Settled tasks evicted from the retired ring, remembered as task id -> its
/// work and its orchestration's work only, so a report of a superseded or
/// abandoned task that arrives after many newer ones still finds its work.
/// Bounded: the oldest goes first.
const MAX_SETTLED_TASKS: usize = 4096;

/// PRD #1790: the most `attempt_session` lines one attempt writes as soon as
/// its session id changes. Past it the latest id is held, not written at
/// once: [`WorkTracker::flush_held_sessions`] writes it at its first round
/// that finds room in the session quota (every [`SESSION_FLUSH_EVERY`] in
/// the daemon, [`spawn_session_flush`]), one line per attempt per round
/// however often the id changed in between, and the attempt's end writes it
/// too ([`WorkTracker::attempt_ended`]), as does an orderly shutdown
/// ([`WorkTracker::drain`]). So a producer that flaps between ids writes at
/// most this many lines, then at most one per round, then one at its end.
pub const MAX_SESSION_LINES: usize = 32;

/// PRD #1790: how often the daemon writes the session ids its tracker holds
/// back ([`spawn_session_flush`]): one refused by the journal's session
/// quota, or one past [`MAX_SESSION_LINES`]. An id the tracker holds reaches
/// the journal's queue at the first round after it was reported that finds
/// room in the session quota, while the daemon runs, and at an orderly
/// shutdown ([`WorkTracker::drain`]).
pub const SESSION_FLUSH_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// PRD #1790: whether `session`, reported by an agent of type `agent` on
/// `pane`, names a session the agent could be resumed into. Pi's hooks report
/// a synthetic `<pane>-session` id that is not Pi's own, so it is not one; an
/// empty id is not one either. OpenCode's id is taken as reported, though its
/// plugin reports every later session in a folder under the folder's first id
/// (the docs say so; a later stage settles which id a resume needs).
pub fn is_resumable_session(agent: &AgentType, pane: Option<&str>, session: &str) -> bool {
    if session.is_empty() || *agent == AgentType::Pi {
        return false;
    }
    !pane.is_some_and(|pane| session == format!("{pane}-session"))
}

/// What [`WorkTracker::launch_recorded`] writes about one launch: the spawn's
/// own view of it, before any wrapping.
#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchDetails<'a> {
    pub command: Option<&'a str>,
    pub shell: Option<&'a str>,
    pub agent_type: Option<&'a AgentType>,
    pub cwd: Option<&'a str>,
    /// The path a prepared start asked for, when its verified directory
    /// had been moved from it and could not be found
    /// ([`crate::launch_records::LaunchRecord::cwd_moved_from`]).
    pub cwd_moved_from: Option<&'a str>,
    pub name: Option<&'a str>,
    pub membership: Option<&'a TabMembership>,
    /// The agent's child process id and when it started, as spawned
    /// ([`crate::launch_records::LaunchRecord::pid`]).
    pub pid: Option<u32>,
    pub pid_start: Option<u64>,
    /// The pane it was started in ([`crate::launch_records::LaunchRecord::pane`]).
    pub pane: Option<&'a str>,
}

/// How a generation's end reads in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptEnd {
    pub how: EndHow,
    pub code: Option<i32>,
    pub cause: Option<EndCause>,
}

impl AttemptEnd {
    /// `termination` as an attempt end. A close while the registry is shutting
    /// down is the daemon interrupting the attempt.
    pub fn of(termination: &Termination, shutting_down: bool) -> Self {
        let plain = |how| AttemptEnd {
            how,
            code: None,
            cause: None,
        };
        match termination {
            Termination::PaneCommandExited { code, .. } => AttemptEnd {
                how: EndHow::Exited,
                code: i32::try_from(*code).ok(),
                cause: None,
            },
            Termination::Closed if shutting_down => AttemptEnd {
                how: EndHow::Interrupted,
                code: None,
                cause: Some(EndCause::DaemonShutdown),
            },
            Termination::Closed => plain(EndHow::Closed),
            Termination::Replaced => plain(EndHow::Replaced),
            Termination::ReplacementFailed => plain(EndHow::ReplacementFailed),
            // A respawn removed it but had published no successor by the
            // bound, and a stream that ended with the child still running:
            // neither says how the attempt ended.
            Termination::ReplacementPending
            | Termination::ConnectionEnded { .. }
            | Termination::Unknown => plain(EndHow::Unknown),
        }
    }
}

struct Delegation {
    work: WorkId,
    orchestration: Option<WorkId>,
    /// The orchestration instance, the worker's pane and its bound
    /// generation: the task is retired when the instance ends, the generation
    /// ends, or a newer task's post supersedes it (by id, see
    /// [`WorkTracker::delegate_posted`]).
    instance: Option<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pane: String,
    worker_agent: String,
    /// The delegation's `opened` and its `attempt_started`, held from before
    /// the post until the post goes in ([`WorkTracker::delegate_posted`]) or
    /// the task's report arrives first, whichever is sooner; dropped with the
    /// delegation when the post is refused.
    held: Vec<Event>,
}

/// What the book knows of a launch whose spawn has not settled.
#[derive(Default)]
struct HeldLaunch {
    /// Something of it is in the journal: its `opened`, an attempt of its
    /// own (a single agent's pane), or a child whose `opened` names it (the
    /// orchestration it started).
    written: bool,
    /// It was closed (its unit ended during the spawn).
    closed: bool,
    /// How it closes when its spawn fails after something was written: with
    /// its own pane, or with the orchestration it started.
    close_as: Option<CloseHow>,
    /// The orchestration's work it started, once that was opened.
    child: Option<WorkId>,
    /// Its `opened` waits for a unit id (a dispatch): it is not written by
    /// the first thing published under it, but once the unit is registered.
    awaits_unit: bool,
}

struct UnitWork {
    work: WorkId,
    caller: Option<WorkId>,
}

#[derive(Default)]
struct Book {
    /// `opened` events minted at a spawn and written when its first attempt
    /// is published, so a refused spawn leaves no work behind.
    pending_open: HashMap<WorkId, Event>,
    /// Orchestration instance id -> its work.
    orchestrations: HashMap<String, WorkId>,
    /// Works a pane's close closes, and how: a pane's own work (`pane_closed`)
    /// and a plain scheduled fire's (`fire_done`). An orchestration's or a
    /// dispatched unit's work has its own end.
    pane_works: HashMap<WorkId, CloseHow>,
    /// Live attempt (registry agent id) -> the work it serves.
    attempts: HashMap<String, WorkId>,
    /// Pane id -> the work its latest attempt served, until the pane closes,
    /// so a pane re-created with no record left keeps its work.
    panes: HashMap<String, WorkId>,
    /// Ends that arrived before their attempt's start (see [`MAX_EARLY_ENDS`]).
    early_ends: HashMap<String, AttemptEnd>,
    /// A dispatch's `opened`, held until its unit is registered.
    pending_units: HashMap<WorkId, Event>,
    /// Launches (a dispatch, a scheduled issue dispatch) whose spawn has not
    /// settled yet ([`WorkTracker::hold_launch`]).
    launches: HashMap<WorkId, HeldLaunch>,
    /// An orchestration's work -> the launch work that closes when it ends
    /// (a scheduled issue dispatch whose unit is an orchestration).
    close_with_child: HashMap<WorkId, WorkId>,
    /// Delegated task id -> its work.
    tasks: HashMap<String, Delegation>,
    /// Tasks retired without a report, newest last (see [`MAX_RETIRED_TASKS`]).
    retired: VecDeque<(String, Delegation)>,
    /// Settled tasks evicted from `retired`: task id -> (its work, its
    /// orchestration's work), see [`MAX_SETTLED_TASKS`]; `settled_order` is
    /// their ids, oldest first.
    settled: HashMap<String, (WorkId, Option<WorkId>)>,
    settled_order: VecDeque<String>,
    /// Spawns in flight (a [`SpawnWork`] not yet dropped) per work whose
    /// `opened` is still pending, so a failed spawn forgets a minted work only
    /// when no other spawn of it is still on its way.
    spawning: HashMap<WorkId, usize>,
    /// Dispatched unit id -> its work.
    units: HashMap<String, UnitWork>,
    /// Live attempt (registry agent id) -> what it wrote as
    /// `attempt_session` (PRD #1790): the last id, so an unchanged id is not
    /// written again, how many lines it wrote ([`MAX_SESSION_LINES`]) and the
    /// latest id it holds back. Retired with the attempt.
    sessions: HashMap<String, SessionBook>,
    /// Reserved generation (registry agent id, its spawn not yet published)
    /// -> the session ids its hooks reported, in order, held until its
    /// attempt starts ([`WorkTracker::attempt_started`] writes them right
    /// after its `attempt_started`) or dropped when its reservation ends
    /// without one ([`WorkTracker::spawn_abandoned`]). An entry is added only
    /// when the caller says the generation is reserved, which the registry
    /// decides under its lock, and the registry clears it in the lock hold that
    /// ends that reservation, so this holds at most one entry per reservation
    /// the registry holds (one more per spawn whose reservation's `Drop` found
    /// the registry lock poisoned, as the reservation itself leaks then).
    /// Each entry keeps at most [`MAX_SESSION_LINES`] ids, the newest.
    early_sessions: HashMap<String, VecDeque<(String, AgentType)>>,
}

/// What one live attempt wrote as `attempt_session` (PRD #1790).
#[derive(Default)]
struct SessionBook {
    /// The id last written.
    last: Option<String>,
    /// How many `attempt_session` lines it wrote.
    written: usize,
    /// The latest id, when it is not the one last written: past the cap, or
    /// refused by the journal's session quota. Written at the first
    /// [`WorkTracker::flush_held_sessions`] round that finds quota room, at
    /// an orderly shutdown's [`WorkTracker::drain`], or when the attempt
    /// ends.
    held: Option<(String, AgentType)>,
}

impl Book {
    /// Move the tasks `which` picks (by id and entry) to the retired ring.
    fn retire_tasks(&mut self, which: impl Fn(&str, &Delegation) -> bool) {
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(id, task)| which(id, task))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(task) = self.tasks.remove(&id) {
                self.retired.push_back((id, task));
            }
        }
        while self.retired.len() > MAX_RETIRED_TASKS {
            // The oldest settled entry; an unsettled one (held events, its
            // post's outcome still on its way) waits for that callback.
            let Some(at) = self
                .retired
                .iter()
                .position(|(_, task)| task.held.is_empty())
            else {
                break;
            };
            if let Some((id, task)) = self.retired.remove(at) {
                self.settle(id, task.work, task.orchestration);
            }
        }
    }

    /// Remember settled task `id`'s work past the retired ring.
    fn settle(&mut self, id: String, work: WorkId, orchestration: Option<WorkId>) {
        if self
            .settled
            .insert(id.clone(), (work, orchestration))
            .is_none()
        {
            self.settled_order.push_back(id);
        }
        while self.settled_order.len() > MAX_SETTLED_TASKS {
            if let Some(oldest) = self.settled_order.pop_front() {
                self.settled.remove(&oldest);
            }
        }
    }

    /// Take settled task `task_id`'s work and its orchestration's.
    fn take_settled(&mut self, task_id: &str) -> Option<(WorkId, Option<WorkId>)> {
        let found = self.settled.remove(task_id)?;
        if let Some(at) = self.settled_order.iter().position(|id| id == task_id) {
            self.settled_order.remove(at);
        }
        Some(found)
    }

    /// Task `task_id`, live or retired.
    fn task_mut(&mut self, task_id: &str) -> Option<&mut Delegation> {
        if self.tasks.contains_key(task_id) {
            return self.tasks.get_mut(task_id);
        }
        self.retired
            .iter_mut()
            .find(|(id, _)| id == task_id)
            .map(|(_, task)| task)
    }

    /// Take task `task_id`, live or retired.
    fn take_task(&mut self, task_id: &str) -> Option<Delegation> {
        if let Some(task) = self.tasks.remove(task_id) {
            return Some(task);
        }
        let at = self.retired.iter().position(|(id, _)| id == task_id)?;
        self.retired.remove(at).map(|(_, task)| task)
    }
}

/// How many entries each of the book's tables holds (a test seam).
#[cfg(test)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BookSizes {
    pub pending_open: usize,
    pub orchestrations: usize,
    pub pane_works: usize,
    pub attempts: usize,
    pub panes: usize,
    pub early_ends: usize,
    pub pending_units: usize,
    pub launches: usize,
    pub close_with_child: usize,
    pub tasks: usize,
    pub retired: usize,
    pub settled: usize,
    pub units: usize,
    pub spawning: usize,
    pub sessions: usize,
    pub early_sessions: usize,
}

/// What a spawn minted, so a spawn that publishes nothing can forget it.
enum Minted {
    /// A pane work of its own.
    Pane,
    /// Orchestration instance `.0`'s work, minted or joined while still
    /// unpublished.
    Orchestration(String),
}

/// The work a spawn serves ([`WorkTracker::resolve_spawn`]). Hold it until
/// the spawn returns: dropped before the spawn's first attempt was published
/// (a refusal, an error, an unwind), it forgets the work the spawn minted —
/// unless another spawn of the same orchestration is still on its way, or
/// already published it.
#[must_use = "dropping it forgets a work the spawn has not published yet"]
pub struct SpawnWork<'a> {
    tracker: &'a WorkTracker,
    work: Option<WorkId>,
    minted: Option<Minted>,
}

impl SpawnWork<'_> {
    /// The work, when the spawn serves one.
    pub fn work(&self) -> Option<&WorkId> {
        self.work.as_ref()
    }

    /// The work, kept whatever happens next: a test standing in for a spawn
    /// that publishes.
    #[cfg(test)]
    pub(crate) fn kept(mut self) -> Option<WorkId> {
        let work = self.work.take();
        if let (Some(work), Some(_)) = (&work, self.minted.take()) {
            let mut book = self.tracker.book();
            if let Some(count) = book.spawning.get_mut(work) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    book.spawning.remove(work);
                }
            }
        }
        work
    }
}

impl Drop for SpawnWork<'_> {
    fn drop(&mut self) {
        let (Some(work), Some(minted)) = (self.work.take(), self.minted.take()) else {
            return;
        };
        let mut book = self.tracker.book();
        let left = match book.spawning.get_mut(&work) {
            Some(count) => {
                *count = count.saturating_sub(1);
                *count
            }
            None => 0,
        };
        if left > 0 {
            return;
        }
        book.spawning.remove(&work);
        // Published: its first attempt wrote the `opened`, and the work lives
        // on with its pane or its orchestration.
        if book.pending_open.remove(&work).is_none() {
            return;
        }
        book.pane_works.remove(&work);
        if let Minted::Orchestration(instance) = minted
            && book.orchestrations.get(&instance) == Some(&work)
        {
            book.orchestrations.remove(&instance);
        }
    }
}

/// The registry's work bookkeeping. See the module docs.
#[derive(Default)]
pub struct WorkTracker {
    journal: OnceLock<WorkJournal>,
    book: Mutex<Book>,
}

impl std::fmt::Debug for WorkTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkTracker")
            .field("journal", &self.journal.get())
            .finish_non_exhaustive()
    }
}

impl WorkTracker {
    /// Install the run's journal. A second install is ignored.
    pub fn install(&self, journal: WorkJournal) {
        if self.journal.set(journal).is_err() {
            tracing::warn!("work journal: one was already installed; keeping it");
        }
    }

    /// An orderly shutdown's journal barrier, best effort: first hand every
    /// live attempt's held-back session id ([`Self::flush_held_sessions`]'s,
    /// which no later round will write) to the writer, outside the session
    /// quota and waiting for queue room rather than dropping it, then wait
    /// for the writer to write what it was handed. On a shutdown whose
    /// writer keeps up, that leaves each live attempt's latest reported id
    /// on disk without relying on the next periodic round or on the
    /// attempt's end, which a PTY reader detached at shutdown may never
    /// reach.
    ///
    /// `bound` caps the waiting for queue room and for the writer's drain,
    /// not the whole call: taking the book's lock and walking its sessions
    /// are outside it, and each wait polls every 5 ms, so the call can
    /// overrun `bound` by about that much. An id still unqueued when the
    /// deadline passes stays held and is logged, not written; one the writer
    /// was handed but did not write in time (or that a failed writer drops)
    /// is missing from disk. A session reported after this call is not
    /// covered.
    pub fn drain(&self, bound: std::time::Duration) {
        let Some(journal) = self.journal() else {
            return;
        };
        let deadline = std::time::Instant::now() + bound;
        let unsent = self.hand_over_held_sessions(journal, deadline);
        if unsent > 0 {
            tracing::warn!(
                unsent,
                "work journal: could not queue every held session id before shutdown"
            );
        }
        if !journal.flush_within(deadline.saturating_duration_since(std::time::Instant::now())) {
            tracing::warn!(
                "work journal: the writer did not drain before shutdown; its last events may be missing"
            );
        }
    }

    /// [`Self::drain`]'s first half: queue each live attempt's held session
    /// id with [`WorkJournal::record_within`] until `deadline`, under the
    /// book's lock so a session reported meanwhile is queued after it, not
    /// before. The journal's writer never takes the book, so waiting for its
    /// queue under the lock cannot deadlock. Answers how many held ids it
    /// could not queue (kept held).
    fn hand_over_held_sessions(
        &self,
        journal: &WorkJournal,
        deadline: std::time::Instant,
    ) -> usize {
        let mut guard = self.book();
        let book = &mut *guard;
        let mut unsent = 0;
        for (agent_id, entry) in &mut book.sessions {
            let Some((session, agent)) = entry.held.take() else {
                continue;
            };
            let (Some(work), Some(attempt)) = (
                book.attempts.get(agent_id),
                agent_id
                    .parse::<u64>()
                    .ok()
                    .and_then(|agent| journal.attempt_id(agent)),
            ) else {
                entry.held = Some((session, agent));
                continue;
            };
            let event = Event::AttemptSession {
                work: work.clone(),
                attempt,
                session: session.clone(),
                agent: agent.clone(),
            };
            if journal.record_within(event, deadline) {
                entry.last = Some(session);
            } else {
                entry.held = Some((session, agent));
                unsent += 1;
            }
        }
        unsent
    }

    /// The run's journal, once installed.
    pub fn journal(&self) -> Option<&WorkJournal> {
        self.journal.get()
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A fresh work id, or `None` with no journal installed.
    pub fn mint(&self) -> Option<WorkId> {
        self.journal()?.mint_work_id()
    }

    fn refs_plain(journal: &WorkJournal, text: Option<&str>) -> Option<String> {
        text.and_then(|t| journal.plain(t))
    }

    /// Mint a work, write its `opened` now, and answer it.
    pub fn open(
        &self,
        launch: Launch,
        parent: Option<&WorkId>,
        refs: Refs,
        workspace: Option<&Path>,
    ) -> Option<WorkId> {
        let journal = self.journal()?;
        let work = journal.mint_work_id()?;
        journal.record(Event::Opened {
            work: work.clone(),
            launch,
            parent: parent.cloned(),
            refs: Box::new(refs),
            workspace: workspace.and_then(|p| journal.workspace(p)),
        });
        Some(work)
    }

    /// Plain (non-path) text for a ref, or `None` when the redaction rule
    /// would refuse it.
    pub fn plain(&self, text: &str) -> Option<String> {
        self.journal()?.plain(text)
    }

    /// The work a spawn serves, decided before the fork so the child can be
    /// handed it.
    ///
    /// - A role pane of an orchestration instance serves the instance's work,
    ///   minted the first time the instance is seen, with `requested` (the
    ///   dispatch or fire that started it) as its parent.
    /// - Any other spawn serves `requested` when the caller gave one (a
    ///   respawn's pinned work, a dispatched unit's, a fire's).
    /// - Otherwise the spawn is a pane of its own: a fresh pane work.
    ///
    /// A work minted here is `opened` when the spawn's first attempt is
    /// published ([`Self::attempt_started`]); the returned guard forgets it if
    /// the spawn ends first.
    pub fn resolve_spawn(
        &self,
        requested: Option<&WorkId>,
        pane: Option<&str>,
        membership: Option<&TabMembership>,
        cwd: Option<&str>,
    ) -> SpawnWork<'_> {
        let guard = |work: Option<WorkId>, minted: Option<Minted>| SpawnWork {
            tracker: self,
            work,
            minted,
        };
        let Some(journal) = self.journal() else {
            return guard(None, None);
        };
        if let Some(TabMembership::Orchestration {
            name,
            orchestration_id: Some(instance),
            orchestration_cwd,
            ..
        }) = membership
        {
            // Resolved before the book's lock: it reads the filesystem.
            let workspace = orchestration_cwd
                .as_deref()
                .or(cwd)
                .and_then(|p| journal.workspace(Path::new(p)));
            let mut book = self.book();
            if let Some(work) = book.orchestrations.get(instance).cloned() {
                if !book.pending_open.contains_key(&work) {
                    return guard(Some(work), None);
                }
                *book.spawning.entry(work.clone()).or_default() += 1;
                return guard(Some(work), Some(Minted::Orchestration(instance.clone())));
            }
            let Some(work) = journal.mint_work_id() else {
                return guard(None, None);
            };
            let refs = Refs {
                orchestration_id: journal.plain(instance),
                orchestration: journal.plain(name),
                ..Refs::default()
            };
            book.pending_open.insert(
                work.clone(),
                Event::Opened {
                    work: work.clone(),
                    launch: Launch::Orchestration,
                    parent: requested.cloned(),
                    refs: Box::new(refs),
                    workspace,
                },
            );
            book.orchestrations.insert(instance.clone(), work.clone());
            *book.spawning.entry(work.clone()).or_default() += 1;
            return guard(Some(work), Some(Minted::Orchestration(instance.clone())));
        }
        if let Some(work) = requested {
            return guard(Some(work.clone()), None);
        }
        let Some(work) = journal.mint_work_id() else {
            return guard(None, None);
        };
        let refs = Refs {
            pane: Self::refs_plain(journal, pane),
            ..Refs::default()
        };
        let workspace = cwd.and_then(|p| journal.workspace(Path::new(p)));
        let mut book = self.book();
        book.pending_open.insert(
            work.clone(),
            Event::Opened {
                work: work.clone(),
                launch: Launch::Pane,
                parent: None,
                refs: Box::new(refs),
                workspace,
            },
        );
        book.pane_works.insert(work.clone(), CloseHow::PaneClosed);
        *book.spawning.entry(work.clone()).or_default() += 1;
        guard(Some(work), Some(Minted::Pane))
    }

    /// How many entries each table of the book holds.
    #[cfg(test)]
    pub(crate) fn book_sizes(&self) -> BookSizes {
        let book = self.book();
        BookSizes {
            pending_open: book.pending_open.len(),
            orchestrations: book.orchestrations.len(),
            pane_works: book.pane_works.len(),
            attempts: book.attempts.len(),
            panes: book.panes.len(),
            early_ends: book.early_ends.len(),
            pending_units: book.pending_units.len(),
            launches: book.launches.len(),
            close_with_child: book.close_with_child.len(),
            tasks: book.tasks.len(),
            retired: book.retired.len(),
            settled: book.settled.len(),
            units: book.units.len(),
            spawning: book.spawning.len(),
            sessions: book.sessions.len(),
            early_sessions: book.early_sessions.len(),
        }
    }

    /// The work of orchestration instance `instance`, when this run knows it.
    pub fn orchestration_work(&self, instance: &str) -> Option<WorkId> {
        self.book().orchestrations.get(instance).cloned()
    }

    /// The work pane `pane`'s latest attempt served, until the pane closed.
    pub fn work_of_pane(&self, pane: &str) -> Option<WorkId> {
        self.book().panes.get(pane).cloned()
    }

    /// The work the live attempt `agent_id` serves.
    pub fn work_of_attempt(&self, agent_id: &str) -> Option<WorkId> {
        self.book().attempts.get(agent_id).cloned()
    }

    /// Generation `agent_id` was published on `pane` serving `work`: write
    /// the work's pending `opened`, then `attempt_started`. An end that
    /// already arrived for it is written right after.
    pub fn attempt_started(
        &self,
        work: &WorkId,
        agent_id: &str,
        pane: Option<&str>,
        role: Option<&str>,
    ) {
        let Some(journal) = self.journal() else {
            return;
        };
        let Ok(agent) = agent_id.parse::<u64>() else {
            return;
        };
        let Some(attempt) = journal.attempt_id(agent) else {
            return;
        };
        // Under the book's lock, so the end of this attempt (taken under the
        // same lock) is written after its start.
        let mut guard = self.book();
        let book = &mut *guard;
        if let Some(opened) = book.pending_open.remove(work) {
            if let Event::Opened {
                parent: Some(parent),
                ..
            } = &opened
                && let Some(launch) = book.launches.get_mut(parent)
            {
                launch.written = true;
                launch.close_as.get_or_insert(CloseHow::OrchestrationEnded);
                launch.child = Some(work.clone());
                // The parent's `opened` before the child's that names it,
                // unless it waits for its unit id.
                if !launch.awaits_unit
                    && let Some(parent_opened) = book.pending_units.remove(parent)
                {
                    journal.record(parent_opened);
                }
            }
            journal.record(opened);
        }
        if let Some(launch) = book.launches.get_mut(work) {
            launch.written = true;
            launch.close_as.get_or_insert(CloseHow::PaneClosed);
            if !launch.awaits_unit
                && let Some(opened) = book.pending_units.remove(work)
            {
                journal.record(opened);
            }
        }
        if let Some(pane) = pane {
            book.panes.insert(pane.to_string(), work.clone());
        }
        journal.record(Event::AttemptStarted {
            work: work.clone(),
            attempt: attempt.clone(),
            pane: pane.and_then(|p| journal.plain(p)).unwrap_or_default(),
            agent,
            role: role.and_then(|r| journal.plain(r)),
        });
        // PRD #1790: the sessions its hooks reported before it was
        // published, in order, right after its start.
        for (session, agent_type) in book.early_sessions.remove(agent_id).unwrap_or_default() {
            Self::note_session(journal, book, agent_id, work, &attempt, session, agent_type);
        }
        match book.early_ends.remove(agent_id) {
            Some(end) => {
                Self::flush_held_session(journal, book, agent_id, work, &attempt);
                journal.record(Event::AttemptEnded {
                    work: work.clone(),
                    attempt,
                    how: end.how,
                    code: end.code,
                    cause: end.cause,
                });
            }
            None => {
                book.attempts.insert(agent_id.to_string(), work.clone());
            }
        }
    }

    /// PRD #1790: the reservation of generation `agent_id` ended without an
    /// attempt starting for it (its spawn was abandoned, or published serving
    /// no work): the session reports held for it go. The registry calls it
    /// under its lock, in the same hold that ends the reservation.
    pub fn spawn_abandoned(&self, agent_id: &str) {
        self.book().early_sessions.remove(agent_id);
    }

    /// Write `session` for live attempt `agent_id` unless it is the id last
    /// written; past [`MAX_SESSION_LINES`], or when the journal's session
    /// quota refuses it, hold it instead, for [`Self::flush_held_sessions`]
    /// and the attempt's end.
    fn note_session(
        journal: &WorkJournal,
        book: &mut Book,
        agent_id: &str,
        work: &WorkId,
        attempt: &crate::work_journal::AttemptId,
        session: String,
        agent: AgentType,
    ) {
        let entry = book.sessions.entry(agent_id.to_string()).or_default();
        if entry.last.as_deref() == Some(session.as_str()) {
            entry.held = None;
            return;
        }
        if entry.written >= MAX_SESSION_LINES {
            entry.held = Some((session, agent));
            return;
        }
        let event = Event::AttemptSession {
            work: work.clone(),
            attempt: attempt.clone(),
            session: session.clone(),
            agent: agent.clone(),
        };
        if journal.record_session(event) {
            entry.last = Some(session);
            entry.written += 1;
            entry.held = None;
        } else {
            entry.held = Some((session, agent));
        }
    }

    /// PRD #1790: write each live attempt's held-back session id
    /// ([`Self::note_session`]), at most one line per attempt, through the
    /// journal's session quota; an id the quota refuses again stays held for
    /// the next round. Answers how many it queued. The daemon runs it every
    /// [`SESSION_FLUSH_EVERY`] ([`spawn_session_flush`]); it takes only the
    /// book's leaf mutex and queues, so it may run on the async runtime.
    ///
    /// What this makes durable, and what it does not: an attempt's latest
    /// admitted id reaches the writer's queue at the first round after it
    /// was reported that finds room in the quota, without waiting for the
    /// attempt to end or for another hook, and the writer writes and syncs
    /// it from there. A daemon that dies before that round, or before the
    /// writer took the line, loses that id, and the journal keeps the one
    /// before it. An orderly shutdown writes it at [`Self::drain`].
    pub fn flush_held_sessions(&self) -> usize {
        let Some(journal) = self.journal() else {
            return 0;
        };
        let mut guard = self.book();
        let book = &mut *guard;
        let mut queued = 0;
        for (agent_id, entry) in &mut book.sessions {
            let Some((session, agent)) = entry.held.take() else {
                continue;
            };
            let (Some(work), Some(attempt)) = (
                book.attempts.get(agent_id),
                agent_id
                    .parse::<u64>()
                    .ok()
                    .and_then(|agent| journal.attempt_id(agent)),
            ) else {
                entry.held = Some((session, agent));
                continue;
            };
            let event = Event::AttemptSession {
                work: work.clone(),
                attempt,
                session: session.clone(),
                agent: agent.clone(),
            };
            if journal.record_session(event) {
                entry.last = Some(session);
                queued += 1;
            } else {
                entry.held = Some((session, agent));
            }
        }
        queued
    }

    /// Retire live attempt `agent_id`'s session book, handing the id it held
    /// back, if any, to the journal so the attempt's last `attempt_session`
    /// line is its latest id. Outside the session quota (one line per
    /// attempt), and best effort as [`WorkJournal::record`] is: a full queue
    /// drops it.
    fn flush_held_session(
        journal: &WorkJournal,
        book: &mut Book,
        agent_id: &str,
        work: &WorkId,
        attempt: &crate::work_journal::AttemptId,
    ) {
        if let Some(SessionBook {
            held: Some((session, agent)),
            ..
        }) = book.sessions.remove(agent_id)
        {
            journal.record(Event::AttemptSession {
                work: work.clone(),
                attempt: attempt.clone(),
                session,
                agent,
            });
        }
    }

    /// PRD #1790: generation `agent_id`'s hooks report it is in agent session
    /// `session`. For a live attempt, writes `attempt_session` the first time
    /// the attempt's id is seen and each time it changes, never for an
    /// unchanged one, at most [`MAX_SESSION_LINES`] times at once, holding the
    /// latest past that (or one the session quota refused) for
    /// [`Self::flush_held_sessions`] and the attempt's end. For a generation
    /// whose spawn is `reserved` (forked, not yet published) the id is held,
    /// in order, and
    /// written right after the attempt's `attempt_started`. Nothing for an
    /// attempt this run is not serving otherwise (one already ended), for an
    /// id the redaction rule refuses, or for an id that names no resumable
    /// session ([`is_resumable_session`]). Takes only the book's leaf mutex
    /// and queues the event, so it may be called under the state's lock that
    /// applied the hook, and under the registry's (which is how `reserved`
    /// is decided: [`crate::agent_pty::AgentPtyRegistry::session_seen`]).
    pub fn session_seen(
        &self,
        agent_id: &str,
        pane: Option<&str>,
        session: &str,
        agent: &AgentType,
        reserved: bool,
    ) {
        let Some(journal) = self.journal() else {
            return;
        };
        if !is_resumable_session(agent, pane, session) {
            return;
        }
        let Some(attempt) = agent_id
            .parse::<u64>()
            .ok()
            .and_then(|agent| journal.attempt_id(agent))
        else {
            return;
        };
        let Some(plain) = journal.plain(session) else {
            return;
        };
        let mut guard = self.book();
        let book = &mut *guard;
        if let Some(work) = book.attempts.get(agent_id).cloned() {
            Self::note_session(
                journal,
                book,
                agent_id,
                &work,
                &attempt,
                plain,
                agent.clone(),
            );
            return;
        }
        if !reserved {
            return;
        }
        let held = book.early_sessions.entry(agent_id.to_string()).or_default();
        if held.back().is_some_and(|(last, _)| *last == plain) {
            return;
        }
        held.push_back((plain, agent.clone()));
        while held.len() > MAX_SESSION_LINES {
            held.pop_front();
        }
    }

    /// PRD #1790: write how generation `agent_id`, serving `work`, was
    /// launched to the launch-record store under the journal's state
    /// directory ([`crate::launch_records`]). File I/O: call it with no lock
    /// held. Nothing is written with no journal installed, or a disabled one
    /// (whose retention could never remove the record); a failure is logged.
    pub fn launch_recorded(&self, work: &WorkId, agent_id: &str, launch: LaunchDetails<'_>) {
        let Some(journal) = self.journal().filter(|j| j.is_enabled()) else {
            return;
        };
        let (Some(state_dir), Some(attempt)) = (
            journal.state_dir(),
            agent_id
                .parse::<u64>()
                .ok()
                .and_then(|agent| journal.attempt_id(agent)),
        ) else {
            return;
        };
        let (role, orchestration, orchestration_id) = match launch.membership {
            Some(TabMembership::Orchestration {
                name,
                role_name,
                orchestration_id,
                ..
            }) => (
                (!role_name.is_empty()).then(|| role_name.clone()),
                Some(name.clone()),
                orchestration_id.clone(),
            ),
            _ => (None, None, None),
        };
        let cwd = launch.cwd.map(|cwd| {
            std::path::absolute(cwd)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| cwd.to_string())
        });
        let record = crate::launch_records::LaunchRecord {
            v: crate::launch_records::SCHEMA_VERSION,
            work: work.clone(),
            attempt,
            command: launch.command.map(str::to_string),
            shell: launch.shell.map(str::to_string),
            agent_type: launch.agent_type.cloned(),
            cwd,
            cwd_moved_from: launch.cwd_moved_from.map(str::to_string),
            name: launch.name.map(str::to_string),
            role,
            orchestration,
            orchestration_id,
            ts_ms: crate::launch_records::now_ms(),
            pid: launch.pid,
            pid_start: launch.pid.and(launch.pid_start),
            pane: launch.pane.map(str::to_string),
        };
        if let Err(error) = crate::launch_records::write(state_dir, &record) {
            tracing::warn!(
                %error,
                agent_id,
                "launch records: could not record how an agent was launched"
            );
        }
    }

    /// Generation `agent_id` ended.
    pub fn attempt_ended(&self, agent_id: &str, end: AttemptEnd) {
        let Some(journal) = self.journal() else {
            return;
        };
        let Some(attempt) = agent_id
            .parse::<u64>()
            .ok()
            .and_then(|agent| journal.attempt_id(agent))
        else {
            return;
        };
        let mut guard = self.book();
        let book = &mut *guard;
        // A task bound to this generation can no longer be worked by it.
        book.retire_tasks(|_, task| task.worker_agent == agent_id);
        let Some(work) = book.attempts.remove(agent_id) else {
            book.sessions.remove(agent_id);
            if book.early_ends.len() < MAX_EARLY_ENDS {
                book.early_ends.insert(agent_id.to_string(), end);
            }
            return;
        };
        // PRD #1790: the latest session id, when it was held back, before
        // the end.
        Self::flush_held_session(journal, book, agent_id, &work, &attempt);
        journal.record(Event::AttemptEnded {
            work,
            attempt,
            how: end.how,
            code: end.code,
            cause: end.cause,
        });
    }

    /// Pane `pane` was closed: the pane work it served closes with it. A work
    /// the pane only served (an orchestration's, a unit's) is left to its own
    /// end.
    pub fn pane_closed(&self, pane: &str) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        let Some(work) = book.panes.remove(pane) else {
            return;
        };
        if let Some(how) = book.pane_works.remove(&work) {
            journal.record(Event::Closed { work, how });
        }
    }

    /// Scheduled fire `work` has done what it does at fire time. When a pane
    /// it spawned is still serving it, it closes (`fire_done`) when that pane
    /// closes; otherwise — it re-posted into a pane an earlier fire opened,
    /// it started an orchestration (whose own work carries on as its child),
    /// its spawn failed, or its pane already closed — it closes now.
    pub fn fire_settled(&self, work: &WorkId) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        if book.panes.values().any(|served| served == work) {
            book.pane_works.insert(work.clone(), CloseHow::FireDone);
            return;
        }
        journal.record(Event::Closed {
            work: work.clone(),
            how: CloseHow::FireDone,
        });
    }

    /// Write `closed` for `work`.
    pub fn close(&self, work: &WorkId, how: CloseHow) {
        if let Some(journal) = self.journal() {
            if let Some(launch) = self.book().launches.get_mut(work) {
                launch.closed = true;
            }
            journal.record(Event::Closed {
                work: work.clone(),
                how,
            });
        }
    }

    /// A delegate is about to be posted as task `task_id`: mint the task's
    /// work, a child of the orchestration's, and register it under the task
    /// id **before** the post, so a report that races the post still finds it.
    /// Its `opened` and the `attempt_started` naming the bound worker
    /// generation are held, not written: [`Self::delegate_posted`] writes them
    /// once the post goes in, [`Self::reported`] writes them first if the
    /// report wins, and [`Self::delegate_refused`] drops them, so a refused
    /// delegate leaves nothing behind (as a refused spawn does).
    pub fn delegate_pending(
        &self,
        instance: Option<&str>,
        task_id: &str,
        role: &str,
        worker_pane: &str,
        worker_agent: &str,
    ) {
        let Some(journal) = self.journal() else {
            return;
        };
        let Some(work) = journal.mint_work_id() else {
            return;
        };
        let orchestration = instance.and_then(|i| self.orchestration_work(i));
        let refs = Refs {
            task_id: journal.plain(task_id),
            role: journal.plain(role),
            ..Refs::default()
        };
        let mut held = vec![Event::Opened {
            work: work.clone(),
            launch: Launch::Delegation,
            parent: orchestration.clone(),
            refs: Box::new(refs),
            workspace: None,
        }];
        if let Some((agent, attempt)) = worker_agent
            .parse::<u64>()
            .ok()
            .and_then(|agent| journal.attempt_id(agent).map(|a| (agent, a)))
        {
            held.push(Event::AttemptStarted {
                work: work.clone(),
                attempt,
                pane: journal.plain(worker_pane).unwrap_or_default(),
                agent,
                role: journal.plain(role),
            });
        }
        self.book().tasks.insert(
            task_id.to_string(),
            Delegation {
                work,
                orchestration,
                instance: instance.map(str::to_string),
                pane: worker_pane.to_string(),
                worker_agent: worker_agent.to_string(),
                held,
            },
        );
    }

    /// Task `task_id`'s delegate was posted: write its held `opened` and
    /// `attempt_started`, unless its report already did, and retire exactly
    /// the tasks the post `superseded` (the ids the inbox's task ledger
    /// settled as superseded by it). A newer task to the same pane that is
    /// still on its way to being posted is not one of them, so it is never
    /// retired here, whichever post completes first.
    ///
    /// The held events are written whether the task is still live or was
    /// retired between the hub accepting the post and this callback (a later
    /// post that superseded it called back first, its worker generation
    /// ended, its orchestration ended): the inbox accepted it, so it ran. A
    /// retired task with held events is not evicted from the ring before this
    /// callback takes them ([`MAX_RETIRED_TASKS`]).
    pub fn delegate_posted(&self, task_id: &str, superseded: &[String]) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        if let Some(task) = book.task_mut(task_id) {
            for event in std::mem::take(&mut task.held) {
                journal.record(event);
            }
        }
        // Whether or not this task is still live (its report may have won):
        // the tasks it superseded are settled either way.
        book.retire_tasks(|id, _| superseded.iter().any(|s| s == id) && id != task_id);
    }

    /// Task `task_id`'s delegate was refused: forget its work, live or
    /// already retired. Nothing of it was written.
    pub fn delegate_refused(&self, task_id: &str) {
        let mut book = self.book();
        if book.take_task(task_id).is_none() {
            book.take_settled(task_id);
        }
    }

    /// A delegate was posted, in one step: [`Self::delegate_pending`] then
    /// [`Self::delegate_posted`], superseding every task already posted to
    /// the same pane, as the inbox's task ledger does.
    #[cfg(test)]
    pub fn delegated(
        &self,
        instance: Option<&str>,
        task_id: &str,
        role: &str,
        worker_pane: &str,
        worker_agent: &str,
    ) {
        let superseded: Vec<String> = self
            .book()
            .tasks
            .iter()
            .filter(|(_, task)| task.pane == worker_pane && task.held.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        self.delegate_pending(instance, task_id, role, worker_pane, worker_agent);
        self.delegate_posted(task_id, &superseded);
    }

    /// A report was committed. A task's report is `reported` on the task's
    /// work (to the orchestration's), then the task's work closes; a unit's
    /// terminal report is `reported` on the unit's work (to its caller's); a
    /// note is `reported` on the producer's work (to the recipient's).
    pub fn reported(&self, report: &Reported<'_>) {
        let Some(journal) = self.journal() else {
            return;
        };
        let Some(report_id) = journal.plain(report.report_id) else {
            return;
        };
        let destination = report.destination;
        match report.about {
            ReportAbout::Task(task_id) => {
                // Under the book's lock, so a `delegate_posted` racing this
                // report cannot write the held events after the report.
                let mut book = self.book();
                let (work, orchestration) = match book.take_task(task_id) {
                    Some(task) => {
                        for event in task.held {
                            journal.record(event);
                        }
                        (task.work, task.orchestration)
                    }
                    // Settled long enough ago to have left the retired ring:
                    // its events were written when its post went in.
                    None => match book.take_settled(task_id) {
                        Some(found) => found,
                        None => return,
                    },
                };
                drop(book);
                journal.record(Event::Reported {
                    work: work.clone(),
                    report_id,
                    task_id: journal.plain(task_id),
                    unit_id: None,
                    destination,
                    to: orchestration,
                });
                journal.record(Event::Closed {
                    work,
                    how: CloseHow::TaskDone,
                });
            }
            ReportAbout::Unit(unit_id) => {
                let unit = {
                    let book = self.book();
                    book.units
                        .get(unit_id)
                        .map(|u| (u.work.clone(), u.caller.clone()))
                };
                let Some((work, caller)) = unit else {
                    return;
                };
                self.flush_open(&work);
                journal.record(Event::Reported {
                    work,
                    report_id,
                    task_id: None,
                    unit_id: journal.plain(unit_id),
                    destination,
                    to: caller,
                });
            }
            ReportAbout::Note {
                producer_agent,
                recipient_agent,
            } => {
                let (work, to) = {
                    let book = self.book();
                    (
                        book.attempts.get(producer_agent).cloned(),
                        recipient_agent.and_then(|a| book.attempts.get(a).cloned()),
                    )
                };
                let Some(work) = work else {
                    return;
                };
                journal.record(Event::Reported {
                    work,
                    report_id,
                    task_id: None,
                    unit_id: None,
                    destination,
                    to,
                });
            }
        }
    }

    /// A `dispatch` is about to spawn its unit: mint the dispatch's work, a
    /// child of `parent` (the dispatcher's work), held until the unit is
    /// registered and its id known ([`Self::unit_registered`]), or written as
    /// it is by [`Self::flush_open`] when the dispatch registers no unit.
    pub fn dispatch_minted(
        &self,
        parent: Option<&WorkId>,
        branch: &str,
        workspace: &Path,
    ) -> Option<WorkId> {
        let work = self.mint_launch()?;
        self.dispatch_held(work, parent, branch, workspace)
    }

    /// [`Self::dispatch_minted`] for a work id the caller minted earlier
    /// ([`Self::mint_launch`]), so the worktree it created could name it.
    pub fn dispatch_held(
        &self,
        work: WorkId,
        parent: Option<&WorkId>,
        branch: &str,
        workspace: &Path,
    ) -> Option<WorkId> {
        let refs = Refs {
            branch: self.plain(branch),
            ..Refs::default()
        };
        self.hold_launch_as(work, Launch::Dispatch, parent, refs, workspace)
    }

    /// A fresh work id for a launch that has not been held yet, or `None`
    /// when there is no journal (PR #2431 round 2). Writes nothing: a launch
    /// minted here and never held leaves no line behind. Minted before the
    /// launch creates its worktree, so the worktree's ownership marker can
    /// name the launch ([`crate::worktree_owner::Creator::for_launch`]).
    pub fn mint_launch(&self) -> Option<WorkId> {
        self.journal()?.mint_work_id()
    }

    /// The unit dispatched as `work` was registered as `unit_id`: its
    /// `opened` gains the unit id and is ready to write. Writes nothing (the
    /// caller may hold the state's write guard, and only the book's leaf
    /// mutex is taken); [`Self::flush_open`] does.
    pub fn unit_registered(&self, work: &WorkId, unit_id: &str) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        let Some(mut opened) = book.pending_units.remove(work) else {
            return;
        };
        let mut caller = None;
        if let Event::Opened { refs, parent, .. } = &mut opened {
            refs.unit_id = journal.plain(unit_id);
            caller.clone_from(parent);
        }
        book.pending_open.insert(work.clone(), opened);
        book.units.insert(
            unit_id.to_string(),
            UnitWork {
                work: work.clone(),
                caller,
            },
        );
    }

    /// Write `work`'s `opened` now, if it is still held.
    pub fn flush_open(&self, work: &WorkId) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        Self::flush_locked(journal, &mut book, work);
    }

    fn flush_locked(journal: &WorkJournal, book: &mut Book, work: &WorkId) {
        if let Some(opened) = book
            .pending_open
            .remove(work)
            .or_else(|| book.pending_units.remove(work))
        {
            journal.record(opened);
            if let Some(launch) = book.launches.get_mut(work) {
                launch.written = true;
            }
        }
    }

    /// Forget a held `opened` for a launch that did not happen.
    pub fn forget(&self, work: &WorkId) {
        let mut book = self.book();
        book.pending_open.remove(work);
        book.pending_units.remove(work);
        book.pane_works.remove(work);
    }

    /// Mint a launch's work — a `dispatch`, or a scheduled issue dispatch —
    /// whose spawn is about to run, a child of `parent`. Its `opened` is held
    /// until the spawn settles: written by the spawn once something of it is
    /// published (or by [`Self::unit_registered`] and [`Self::flush_open`]
    /// once a dispatch's unit id is known), by [`Self::launch_succeeded`] or
    /// [`Self::launched_without_unit`] when the spawn returns, and forgotten by
    /// [`Self::launch_failed`] when nothing of it was written.
    pub fn hold_launch(
        &self,
        launch: Launch,
        parent: Option<&WorkId>,
        refs: Refs,
        workspace: &Path,
    ) -> Option<WorkId> {
        let work = self.mint_launch()?;
        self.hold_launch_as(work, launch, parent, refs, workspace)
    }

    /// [`Self::hold_launch`] for a work id the caller minted earlier
    /// ([`Self::mint_launch`]). `None`, holding nothing, when there is no
    /// journal.
    pub fn hold_launch_as(
        &self,
        work: WorkId,
        launch: Launch,
        parent: Option<&WorkId>,
        refs: Refs,
        workspace: &Path,
    ) -> Option<WorkId> {
        let journal = self.journal()?;
        // Resolved before the book's lock: it reads the filesystem.
        let workspace = journal.workspace(workspace);
        let mut book = self.book();
        book.pending_units.insert(
            work.clone(),
            Event::Opened {
                work: work.clone(),
                launch,
                parent: parent.cloned(),
                refs: Box::new(refs),
                workspace,
            },
        );
        book.launches.insert(
            work.clone(),
            HeldLaunch {
                awaits_unit: launch == Launch::Dispatch,
                ..HeldLaunch::default()
            },
        );
        Some(work)
    }

    /// Launch `work`'s spawn succeeded and registered a unit: its `opened` is
    /// written if it is still held. It closes when that unit ends
    /// ([`Self::unit_ended`]).
    pub fn launch_succeeded(&self, work: &WorkId) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        Self::flush_locked(journal, &mut book, work);
        book.launches.remove(work);
    }

    /// Launch `work`'s spawn succeeded and registered no unit whose end
    /// closes it — a scheduled issue dispatch, or a `dispatch` with no caller:
    /// its `opened` is written if it is still held, and it closes with what
    /// it started — a single agent's pane (`pane_closed`, when that pane
    /// closes) or its orchestration (`orchestration_ended`, when that
    /// orchestration ends); when neither is still running, it closes now.
    pub fn launched_without_unit(&self, work: &WorkId) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        Self::flush_locked(journal, &mut book, work);
        let launch = book.launches.remove(work).unwrap_or_default();
        if book.panes.values().any(|served| served == work) {
            book.pane_works.insert(work.clone(), CloseHow::PaneClosed);
            return;
        }
        if let Some(child) = launch.child
            && book.orchestrations.values().any(|o| *o == child)
        {
            book.close_with_child.insert(child, work.clone());
            return;
        }
        if launch.closed {
            return;
        }
        let how = launch.close_as.unwrap_or(CloseHow::PaneClosed);
        drop(book);
        self.close(work, how);
    }

    /// Launch `work`'s spawn failed. When nothing of it was written, it is
    /// forgotten, as a launch that did not happen. When something was — an
    /// attempt of its own, or the orchestration it started, whose `opened`
    /// names it as parent, before a later step failed — its `opened` is
    /// written, so the orchestration's parent can be found, and
    /// it closes `how` (or as what it started ended, with `how` `None`),
    /// unless it already closed.
    pub fn launch_failed(&self, work: &WorkId, how: Option<CloseHow>) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        let launch = book.launches.remove(work).unwrap_or_default();
        if !launch.written {
            drop(book);
            self.forget(work);
            return;
        }
        Self::flush_locked(journal, &mut book, work);
        book.pane_works.remove(work);
        if launch.closed {
            return;
        }
        let how = how.or(launch.close_as).unwrap_or(CloseHow::PaneClosed);
        drop(book);
        self.close(work, how);
    }

    /// PR #2431 round 5: launch `work`'s spawn was cut short by this daemon
    /// shutting down ([`crate::spawn::SpawnError::ShutdownWhileStarting`]).
    /// When something of it was written, its `opened` is written if it is
    /// still held and it stays open: the attempts the shutdown interrupted
    /// list it as interrupted work after the restart. Answers whether it was
    /// kept; when nothing of it was written it is forgotten, as
    /// [`Self::launch_failed`] forgets it, since nothing lists it then.
    pub fn launch_interrupted(&self, work: &WorkId) -> bool {
        let Some(journal) = self.journal() else {
            return false;
        };
        let mut book = self.book();
        let launch = book.launches.remove(work).unwrap_or_default();
        if !launch.written {
            drop(book);
            self.forget(work);
            return false;
        }
        Self::flush_locked(journal, &mut book, work);
        !launch.closed
    }

    /// Orchestration instance `instance` lost its last role pane: its work
    /// closes, and a later start of the same instance id opens a new one.
    pub fn orchestration_ended(&self, instance: &str) {
        let Some(journal) = self.journal() else {
            return;
        };
        let mut book = self.book();
        book.retire_tasks(|_, task| task.instance.as_deref() == Some(instance));
        let Some(work) = book.orchestrations.remove(instance) else {
            return;
        };
        // Opened only once an attempt of it was published; one that never
        // was leaves nothing to close.
        let parent = book.close_with_child.remove(&work);
        if book.pending_open.remove(&work).is_none() {
            journal.record(Event::Closed {
                work,
                how: CloseHow::OrchestrationEnded,
            });
        }
        // A scheduled issue dispatch whose unit was this orchestration.
        if let Some(parent) = parent {
            journal.record(Event::Closed {
                work: parent,
                how: CloseHow::OrchestrationEnded,
            });
        }
    }

    /// Dispatched unit `unit_id` ended: its work closes.
    pub fn unit_ended(&self, unit_id: &str) {
        let unit = self.book().units.remove(unit_id);
        if let Some(unit) = unit {
            self.close(&unit.work, CloseHow::UnitEnded);
        }
    }
}

/// How often compaction runs after the one at startup.
pub const COMPACTION_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The longest a compaction round that startup recovery held back waits for
/// it in one go, on the blocking pool, before it looks again. Short, so a
/// runtime shutting down is not kept waiting on it.
const RECOVERY_WAIT_SLICE: std::time::Duration = std::time::Duration::from_secs(1);

/// Compact `journal` now, off the caller's path, and then every
/// [`COMPACTION_EVERY`]. Compaction blocks, so it runs on the blocking pool.
///
/// Compaction waits for startup recovery's first attempt, which runs on its
/// own thread. A round that finds it still pending is not lost to the next
/// hourly tick: it waits for that attempt (in short slices, off the async
/// path) and runs as soon as it has.
pub fn spawn_compaction(journal: WorkJournal) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(COMPACTION_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            loop {
                let round = journal.clone();
                let Ok(report) = tokio::task::spawn_blocking(move || {
                    let report = round.compact();
                    if !report.skipped_recovery_pending {
                        prune_launch_records(&round);
                    }
                    report
                })
                .await
                else {
                    break;
                };
                if !report.skipped_recovery_pending {
                    break;
                }
                let waiting = journal.clone();
                if tokio::task::spawn_blocking(move || {
                    waiting.wait_recovery_attempted(RECOVERY_WAIT_SLICE)
                })
                .await
                .is_err()
                {
                    break;
                }
            }
        }
    })
}

/// PRD #1790: run `flush` every [`SESSION_FLUSH_EVERY`] until it answers
/// `false` (the daemon passes one that runs
/// [`WorkTracker::flush_held_sessions`] on its registry's tracker, and
/// answers `false` once the registry is gone).
pub fn spawn_session_flush(
    flush: impl Fn() -> bool + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(SESSION_FLUSH_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; there is nothing held yet.
        every.tick().await;
        loop {
            every.tick().await;
            if !flush() {
                return;
            }
        }
    })
}

/// Where the daemon's next launch-record prune lists from
/// ([`crate::launch_records::PruneWindow`]); one daemon, one store.
static LAUNCH_PRUNE_WINDOW: crate::launch_records::PruneWindow =
    crate::launch_records::PruneWindow::new();

/// PRD #1790: remove the launch records whose work `journal` no longer holds
/// ([`crate::launch_records::prune`]), after a compaction round. Blocking;
/// never fails: an error is logged.
pub fn prune_launch_records(journal: &WorkJournal) {
    let Some(state_dir) = journal.state_dir().filter(|_| journal.is_enabled()) else {
        return;
    };
    let read = match journal.read() {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(%error, "launch records: the journal could not be read to prune them");
            return;
        }
    };
    let works: std::collections::HashSet<WorkId> =
        read.lines.iter().map(|l| l.event.work().clone()).collect();
    match crate::launch_records::prune(
        state_dir,
        &works,
        // A line of a newer schema, or of a kind this build does not know,
        // may hold a work this build cannot see, so such a read cannot show
        // a work is gone either.
        launch_read_incomplete(&read.stats),
        crate::launch_records::now_ms(),
        |run| journal.run_is_live(run),
        &LAUNCH_PRUNE_WINDOW,
    ) {
        Ok(report) if report.removed > 0 || report.pending_removed > 0 => {
            tracing::info!(?report, "launch records: pruned");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "launch records: could not prune"),
    }
}

/// PRD #1790: a journal read whose absence of a work the launch-record prune
/// does not act on. Exactly three conditions: the read was truncated, or it
/// skipped a line of a newer schema, or a line of a kind this build does not
/// know; any of them may hold the work. A line skipped as unparsable, too
/// long or torn is not one of them.
pub(crate) fn launch_read_incomplete(stats: &crate::work_journal::ReadStats) -> bool {
    stats.truncated || stats.newer_version > 0 || stats.unknown_kind > 0
}

/// What a committed report was about.
#[derive(Debug, Clone, Copy)]
pub enum ReportAbout<'a> {
    Task(&'a str),
    Unit(&'a str),
    Note {
        producer_agent: &'a str,
        recipient_agent: Option<&'a str>,
    },
}

/// A committed report, as [`WorkTracker::reported`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct Reported<'a> {
    pub report_id: &'a str,
    pub about: ReportAbout<'a>,
    pub destination: Destination,
}

#[cfg(test)]
#[path = "work_tracking_tests.rs"]
mod tests;
