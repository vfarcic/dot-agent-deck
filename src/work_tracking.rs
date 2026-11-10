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
//! hub or other ranked lock held.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use crate::agent_pty::{TabMembership, Termination};
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
    /// Delegated task id -> its work.
    tasks: HashMap<String, Delegation>,
    /// Tasks retired without a report, newest last (see [`MAX_RETIRED_TASKS`]).
    retired: VecDeque<(String, Delegation)>,
    /// Spawns in flight (a [`SpawnWork`] not yet dropped) per work whose
    /// `opened` is still pending, so a failed spawn forgets a minted work only
    /// when no other spawn of it is still on its way.
    spawning: HashMap<WorkId, usize>,
    /// Dispatched unit id -> its work.
    units: HashMap<String, UnitWork>,
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
            self.retired.remove(at);
        }
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
    pub tasks: usize,
    pub retired: usize,
    pub units: usize,
    pub spawning: usize,
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

    /// Wait at most `bound` for the journal's writer to write what it was
    /// handed (a graceful shutdown).
    pub fn drain(&self, bound: std::time::Duration) {
        if let Some(journal) = self.journal()
            && !journal.flush_within(bound)
        {
            tracing::warn!(
                "work journal: the writer did not drain before shutdown; its last events may be missing"
            );
        }
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
            tasks: book.tasks.len(),
            retired: book.retired.len(),
            units: book.units.len(),
            spawning: book.spawning.len(),
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
        let mut book = self.book();
        if let Some(opened) = book.pending_open.remove(work) {
            journal.record(opened);
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
        match book.early_ends.remove(agent_id) {
            Some(end) => journal.record(Event::AttemptEnded {
                work: work.clone(),
                attempt,
                how: end.how,
                code: end.code,
                cause: end.cause,
            }),
            None => {
                book.attempts.insert(agent_id.to_string(), work.clone());
            }
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
        let mut book = self.book();
        // A task bound to this generation can no longer be worked by it.
        book.retire_tasks(|_, task| task.worker_agent == agent_id);
        let Some(work) = book.attempts.remove(agent_id) else {
            if book.early_ends.len() < MAX_EARLY_ENDS {
                book.early_ends.insert(agent_id.to_string(), end);
            }
            return;
        };
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
        self.book().take_task(task_id);
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
                let Some(task) = book.take_task(task_id) else {
                    return;
                };
                for event in task.held {
                    journal.record(event);
                }
                drop(book);
                journal.record(Event::Reported {
                    work: task.work.clone(),
                    report_id,
                    task_id: journal.plain(task_id),
                    unit_id: None,
                    destination,
                    to: task.orchestration,
                });
                journal.record(Event::Closed {
                    work: task.work,
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
        let journal = self.journal()?;
        let work = journal.mint_work_id()?;
        let refs = Refs {
            branch: journal.plain(branch),
            ..Refs::default()
        };
        // Resolved before the book's lock: it reads the filesystem.
        let workspace = journal.workspace(workspace);
        self.book().pending_units.insert(
            work.clone(),
            Event::Opened {
                work: work.clone(),
                launch: Launch::Dispatch,
                parent: parent.cloned(),
                refs: Box::new(refs),
                workspace,
            },
        );
        Some(work)
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
        }
    }

    /// Forget a held `opened` for a launch that did not happen.
    pub fn forget(&self, work: &WorkId) {
        let mut book = self.book();
        book.pending_open.remove(work);
        book.pending_units.remove(work);
        book.pane_works.remove(work);
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
        if book.pending_open.remove(&work).is_none() {
            journal.record(Event::Closed {
                work,
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

/// Compact `journal` now, off the caller's path, and then every
/// [`COMPACTION_EVERY`]. Compaction blocks, so it runs on the blocking pool.
pub fn spawn_compaction(journal: WorkJournal) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(COMPACTION_EVERY);
        loop {
            every.tick().await;
            let journal = journal.clone();
            let _ = tokio::task::spawn_blocking(move || {
                journal.compact();
            })
            .await;
        }
    })
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
