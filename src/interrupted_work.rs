//! PRD #1790 stage 2: the interrupted work list, derived from the work
//! journal, the launch records and the kept report files.
//!
//! It works on what its callers hand it: it opens no file and runs no Git and
//! no forge query (the daemon adds Git and PR facts in a separate step,
//! [`crate::interrupted_work_daemon`], per the journal's authority boundary in
//! `docs/develop/work-journal.md`). The one filesystem access is resolving a
//! workspace, which compares paths in their canonical form as the journal's
//! redaction does. It also holds the wire shapes of the stage 2 daemon verbs
//! and the words every client shows for them (decision D2).
//!
//! # What counts as interrupted
//!
//! An attempt is **interrupted** when its `attempt_ended` (keyed by attempt id
//! across the whole read, never by line order) says `interrupted`, or when it
//! has no `attempt_ended` at all and its run is not live: startup recovery
//! runs in the background, so a dead run's open attempt may not have its end
//! written yet. A work is interrupted when one of its attempts is and the
//! work has no `closed` line.
//!
//! # Items
//!
//! Works roll up to one **item** per root work. From a work, the walk goes up
//! its `parent` while the step stays inside one piece of work: a delegation
//! into its orchestration, and an orchestration into the dispatch or
//! scheduled issue dispatch that started it. Any other parent (the
//! dispatcher's pane or orchestration, a scheduled fire) is the item's
//! **launcher**, not part of it. An item is listed when a work under it is
//! interrupted and no work under it has a live attempt, or when it carries a
//! report left for a caller that is gone (decision D3), and never when its
//! root was closed as dismissed or reclaimed.
//!
//! # Reports left for a caller that is gone
//!
//! A `reported` line's report is joined to its retained file by report id. It
//! was left for a caller that is gone when the file was not acknowledged and
//! either was adopted from an ended run in state pinned, recovered or
//! unlisted, or is this run's own file in state recovered (the inbox accepted
//! it and set it aside when its caller went away, so its line still says
//! `inbox`), or the line says it went to recovery. It is listed under the item
//! of the work that produced it, closed or not.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::launch_records::{LaunchRead, LaunchRecord};
use crate::retained_store::KeptReport;
use crate::work_journal::{
    AttemptId, CloseHow, Destination, EndHow, Event, JournalRead, Launch, Refs, WorkId, Workspace,
};

/// The most `parent` steps a walk takes, so a malformed journal with a cycle
/// or a very long chain cannot loop.
const MAX_WALK: usize = 64;

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A dispatched unit (`launch=dispatch`), single or orchestration-shaped.
    Unit,
    /// An orchestration started from a client or by a scheduled fire.
    Orchestration,
    /// A scheduled issue dispatch.
    IssueDispatch,
    /// A plain agent: a pane, or a scheduled fire's pane, or a work whose
    /// `opened` is not in the read.
    Agent,
}

/// The item's last journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastEntry {
    /// The line's `kind`, as the journal writes it (`attempt_ended`, …).
    pub kind: String,
    pub ts_ms: u64,
}

/// A report kept for the item that the caller it was for never read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemReport {
    pub report_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<String>,
    /// When the deck removes the report's file, in milliseconds since the
    /// Unix epoch; `null` while it is held with no deadline yet.
    #[serde(default)]
    pub kept_until_ms: Option<u64>,
}

/// One entry of the interrupted work list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedItem {
    /// The root work's id: the item's stable id.
    pub id: WorkId,
    pub kind: ItemKind,
    /// A display name: the unit's or agent's pane name, or the
    /// orchestration's name.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub issue: Option<u64>,
    /// `owner/name`.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// The absolute path of the directory the work ran in (a dispatched
    /// unit's worktree), as the journal and launch records recorded it; not
    /// checked to exist.
    #[serde(default)]
    pub worktree: Option<String>,
    /// What started it: the dispatcher's pane or orchestration name, or a
    /// schedule's name; `null` when it was started from a client.
    #[serde(default)]
    pub launcher: Option<String>,
    pub last_entry: LastEntry,
    #[serde(default)]
    pub reports: Vec<ItemReport>,
    /// Kept reports of the item left out of [`Self::reports`] to keep the
    /// reply bounded ([`fit_list`], [`fit_show_page`]), those before a page
    /// included; list them a page at a time with
    /// [`crate::daemon_protocol::AttachRequest::ShowInterrupted`]'s
    /// `reports_from`, and read one by its id with its `report`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reports_omitted: usize,
    /// A name, path or other string of the item was longer than
    /// [`MAX_FIELD_BYTES`] and was cut to it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    /// How far the item's work got, from the journal.
    #[serde(default)]
    pub progress: Progress,
    /// Live Git and pull request facts about the item's worktree and branch,
    /// read when the list was asked for with them (`git: true`); absent
    /// otherwise, so the plain list's shape does not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitFacts>,
    /// PRD #1790 stage 3: the agents that served the item, one per distinct
    /// attempt, at most [`MAX_ITEM_AGENTS`], each with whether it can be
    /// restored. Empty from a daemon before stage 3.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<ItemAgent>,
    /// The item's agents left out of [`Self::agents`] past
    /// [`MAX_ITEM_AGENTS`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub agents_omitted: usize,
}

/// PRD #1790 stage 3: the most agents one item lists.
pub const MAX_ITEM_AGENTS: usize = 16;

/// PRD #1790 stage 3: one agent of an interrupted item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemAgent {
    /// The attempt (`<run>/<agent id>`): what a restore or start-over names.
    pub attempt: AttemptId,
    /// Its pane's display name, from its launch record.
    #[serde(default)]
    pub name: Option<String>,
    /// The agent it ran, from its launch record or the session it reported.
    #[serde(default)]
    pub agent_type: Option<crate::event::AgentType>,
    /// Its role, for an orchestration's agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The folder it ran in, from its launch record; not checked to exist.
    #[serde(default)]
    pub folder: Option<String>,
    /// Whether it can be restored, from the journal and the launch records
    /// alone: the folder, the old process and the saved task are checked
    /// when a restore or start-over is asked for.
    pub restore: RestoreVerdict,
}

/// PRD #1790 stage 3: whether an agent can be restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestoreVerdict {
    /// The deck holds a conversation id the agent can be resumed into.
    Resume,
    /// It cannot be resumed, but its command and folder can start it again
    /// without its conversation.
    StartOver { reason: RestoreReason },
    /// Neither.
    #[serde(rename = "none")]
    NotRestorable { reason: RestoreReason },
    /// A verdict of a newer daemon's.
    #[serde(other)]
    Unknown,
}

impl RestoreVerdict {
    /// Why it cannot be resumed; `None` for [`RestoreVerdict::Resume`].
    pub fn reason(self) -> Option<RestoreReason> {
        match self {
            RestoreVerdict::Resume => None,
            RestoreVerdict::StartOver { reason } | RestoreVerdict::NotRestorable { reason } => {
                Some(reason)
            }
            RestoreVerdict::Unknown => Some(RestoreReason::Unknown),
        }
    }
}

/// PRD #1790 stage 3: why an agent cannot be resumed. The wire codes are
/// kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreReason {
    /// The agent is one the deck cannot resume by id yet, or an
    /// unrecognised command.
    AgentCannotResume,
    /// The agent never reported a conversation id the deck kept.
    NoSessionId,
    /// The conversation id the deck holds is not of the agent's own syntax.
    SessionIdInvalid,
    /// No launch record says how the agent was started.
    NoLaunchRecord,
    /// More than one launch record could be the agent's, or the journal
    /// holds its attempt under more than one piece of work, or two of the
    /// item's agents were in one conversation.
    AmbiguousRecord,
    /// The folder it started in had been moved away.
    FolderMoved,
    /// Its launch record names no folder it started in.
    FolderUnknown,
    /// It served an orchestration, or a unit run by more than one agent,
    /// which a later stage restores.
    OrchestrationLater,
    /// An orchestration's worker that starts fresh for each task.
    ClearWorker,
    /// The pane ran the default shell, not an agent.
    DefaultShell,
    /// The journal or launch-record read was incomplete.
    IncompleteRead,
    /// A restart did not stop it: it ended on its own, or is still running.
    NotInterrupted,
    /// A reason of a newer daemon's.
    #[serde(other)]
    Unknown,
}

/// How far an item's work got, from its journal lines alone: how many
/// attempts ran under it, the roles they ran as, and how many tasks were
/// delegated and how many of those were closed as done (`closed
/// how=task_done`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Distinct attempts across every work of the item.
    #[serde(default)]
    pub attempts: u64,
    /// The roles those attempts ran as, in the order the journal first names
    /// each (an `attempt_started`'s role, or the role a delegation's `opened`
    /// names), across all of the item's works, at most [`MAX_PROGRESS_ROLES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Delegated tasks (`opened launch=delegation`) under the item.
    #[serde(default)]
    pub delegations: u64,
    /// Of those, the ones closed as done.
    #[serde(default)]
    pub delegations_done: u64,
}

/// The most roles a [`Progress`] names.
pub const MAX_PROGRESS_ROLES: usize = 16;

/// What Git (and `gh`) said about an item's worktree and branch when it was
/// listed. Every fact is `None` when it could not be read within its bound,
/// or does not apply (a worktree that is gone has no dirty state).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFacts {
    /// Whether the worktree directory is on disk.
    #[serde(default)]
    pub worktree_exists: Option<bool>,
    /// Whether `git status` reports uncommitted changes in it.
    #[serde(default)]
    pub dirty: Option<bool>,
    /// How many commits its `HEAD` has that no remote-tracking branch holds.
    #[serde(default)]
    pub unpushed_commits: Option<u64>,
    /// Whether the item's branch is a local branch of the repository.
    #[serde(default)]
    pub branch_exists: Option<bool>,
    /// The state of the branch's pull request.
    #[serde(default)]
    pub pr: PrFact,
}

/// The state of an item's pull request, as `gh` reported it within its
/// bound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrFact {
    Open,
    Merged,
    /// Closed without being merged.
    Closed,
    /// `gh` found no pull request for the branch.
    None,
    /// Not asked, or `gh` did not answer within its bound.
    #[default]
    #[serde(other)]
    Unknown,
}

/// The answer to showing one item: the item and the bodies of its kept
/// reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedShow {
    pub item: InterruptedItem,
    /// The item's kept reports, at most [`MAX_SHOWN_REPORTS`], in the item's
    /// order.
    #[serde(default)]
    pub reports: Vec<ShownReport>,
    /// The listing that found the item was incomplete
    /// ([`InterruptedList::incomplete`]).
    #[serde(default)]
    pub incomplete: bool,
    /// Bodies of the first [`MAX_SHOWN_REPORTS`] reports left out because
    /// the reply would have been too large ([`fit_show`]); read each by its
    /// id.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bodies_omitted: usize,
    /// The index, among all of the item's kept reports, of the first report
    /// entry [`InterruptedItem::reports`] carries here: the page asked for
    /// with `reports_from` ([`fit_show_page`]); `0` for the first page.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reports_from: usize,
    /// Where the next page of the item's kept reports starts, to ask for it
    /// with `reports_from`; `None` when this answer reaches the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reports_from: Option<usize>,
}

/// One kept report's body, for showing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownReport {
    pub report_id: String,
    /// The report, framed as untrusted text written by an agent; `None` when
    /// it could not be read (see `error`).
    #[serde(default)]
    pub body: Option<String>,
    /// The file was longer than [`MAX_SHOWN_REPORT_BYTES`], so `body` holds
    /// its start only.
    #[serde(default)]
    pub cut: bool,
    /// Why the body could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The most reports a show returns bodies for.
pub const MAX_SHOWN_REPORTS: usize = 16;

/// The most of each report's file a show reads.
pub const MAX_SHOWN_REPORT_BYTES: u64 = 64 * 1024;

/// The most items a list answers with; [`InterruptedList::omitted`] counts
/// the rest.
pub const MAX_LISTED_ITEMS: usize = 2_000;

/// The most bytes a list's, a show's or a dismissal's JSON answer may take:
/// half the attach protocol's 16 MiB frame bound, leaving room for the
/// response's envelope and the Git facts a list adds after it is fitted
/// ([`GIT_FACTS_RESERVE`]).
pub const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// The bytes a list sets aside per item for the Git facts it adds after
/// fitting: a [`GitFacts`] with every field set serialises far below this.
pub const GIT_FACTS_RESERVE: usize = 512;

/// The most report entries each item of a list carries;
/// [`InterruptedItem::reports_omitted`] counts the rest.
pub const MAX_LISTED_REPORTS: usize = 16;

/// The most bytes a show's report entries (not their bodies) may take.
pub const MAX_SHOWN_ENTRY_BYTES: usize = 1024 * 1024;

/// The longest string of an item a reply carries, in bytes. A journal line
/// is at most 4 KiB, so only a launch record's longer display string is ever
/// cut.
pub const MAX_FIELD_BYTES: usize = 4096;

/// Why a reclaim or dismissal was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalReason {
    /// The request carried an agent's pane claim (decision D4). A guard for
    /// a cooperative agent, not an authentication boundary: see
    /// [`crate::interrupted_work_daemon`].
    AgentCaller,
    /// The request named nothing, or named items and `all` together.
    BadRequest,
    /// The request named more items, or longer ids, than one request may
    /// ([`crate::interrupted_work_daemon::MAX_DISMISS_IDS`] and its
    /// siblings).
    TooLarge,
    /// The journal could not be read.
    Unreadable,
    /// The journal read stopped at its budget, so the item's state is not
    /// known for certain.
    IncompleteRead,
    /// No listed item has that id, name or branch, or the item is no longer
    /// interrupted.
    NotListed,
    /// More than one listed item matches.
    Ambiguous,
    /// A plain agent's item, which has no worktree the deck created
    /// (decision D1): it can be dismissed, not reclaimed.
    PlainAgent,
    /// The item names no worktree, or it is not on disk.
    NoWorktree,
    /// The directory is not a linked worktree's top level.
    NotLinkedWorktree,
    /// The worktree's checked-out branch is not the item's.
    BranchMismatch,
    /// The worktree carries no deck ownership marker.
    NotOwned,
    /// The worktree's ownership marker names another launch than this item
    /// (PR #2431 review, round 2): the work was dispatched again at the same
    /// path and on the same branch, and that later run created the tree that
    /// is there now.
    NewerRun,
    /// The worktree's ownership marker names no launch the deck can match to
    /// this item (PR #2431 round 2): an older build wrote it, the launch had
    /// no work id to record, or the marker could not be read. Whether the
    /// tree is still this item's cannot be confirmed, so it is kept.
    OwnerUnconfirmed,
    /// Something this daemon runs is still rooted in the worktree.
    StillInUse,
    /// Whether something this daemon runs is rooted in the worktree could not
    /// be told: a directory an agent runs in could not be resolved.
    InUseUnknown,
    /// The journal could not record the close.
    NotRecorded,
    #[serde(other)]
    Unknown,
}

/// A refusal and the sentence that explains it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub reason: RefusalReason,
    pub message: String,
}

impl Refusal {
    pub fn new(reason: RefusalReason) -> Self {
        Refusal {
            reason,
            message: refusal_sentence(reason).to_string(),
        }
    }
}

/// What a reclaim did, or would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReclaimVerdict {
    /// The worktree was removed and the branch kept.
    Removed,
    /// A dry run: the worktree would be removed.
    WouldRemove,
    /// Kept: it has uncommitted changes.
    KeptDirty,
    /// Kept: other worktrees lie inside it (decision D6).
    KeptHoldsWorktrees,
    /// Kept: whether it has uncommitted changes could not be checked.
    KeptCouldNotCheck,
    /// Kept: `git worktree remove` failed.
    RemoveFailed,
    /// The removal did not answer within its bound.
    TimedOut,
    /// Kept: just before removing it, the worktree was no longer the one the
    /// reclaim checked (another directory at its path, a missing owner
    /// marker, or another branch checked out), so nothing was removed.
    KeptChanged,
    /// Kept: the repository's worktree lock, which worktree creation and
    /// removal take, was not obtained before the reclaim's wait ran out, so
    /// nothing was removed. Not proof that one operation held it throughout:
    /// the wait samples the lock, so holders taking turns end here too.
    KeptBusy,
    /// Kept: the repository's worktree lock could not be taken for another
    /// reason than another operation holding it (its path could not be
    /// resolved, or its file could not be opened or locked), so nothing was
    /// removed.
    KeptCouldNotLock,
    /// Refused before anything was checked or removed; see
    /// [`ReclaimOutcome::refusal`].
    Refused,
    #[serde(other)]
    Unknown,
}

/// The answer to a reclaim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimOutcome {
    /// The item's id, when the request resolved to one.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    pub verdict: ReclaimVerdict,
    /// The verdict's sentence ([`reclaim_sentence`]), or the refusal's.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Commits on the branch that no remote holds, counted before removal.
    /// The branch keeps them; it never blocks a reclaim (decision D5).
    #[serde(default)]
    pub unpushed_commits: Option<u64>,
    /// The worktrees inside it, for [`ReclaimVerdict::KeptHoldsWorktrees`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_worktrees: Vec<String>,
    /// For [`ReclaimVerdict::Removed`]: the journal recorded the item as
    /// reclaimed, so it is no longer listed. `false` means it will be listed
    /// again although its worktree is gone.
    #[serde(default)]
    pub recorded: bool,
}

impl ReclaimOutcome {
    /// A refusal for `reason`.
    pub fn refused(id: Option<String>, dry_run: bool, reason: RefusalReason) -> Self {
        let refusal = Refusal::new(reason);
        ReclaimOutcome {
            id,
            dry_run,
            verdict: ReclaimVerdict::Refused,
            message: refusal.message.clone(),
            refusal: Some(refusal),
            worktree: None,
            branch: None,
            unpushed_commits: None,
            nested_worktrees: Vec::new(),
            recorded: false,
        }
    }
}

/// One dismissed item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissedItem {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// One id a dismissal refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissRefused {
    /// The id, name or branch as the request gave it.
    pub id: String,
    pub refusal: Refusal,
}

/// The answer to a dismissal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissOutcome {
    #[serde(default)]
    pub dismissed: Vec<DismissedItem>,
    #[serde(default)]
    pub refused: Vec<DismissRefused>,
    /// The whole request was refused and nothing was dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
    /// The listing the ids were matched against was incomplete.
    #[serde(default)]
    pub incomplete: bool,
    /// Items dismissed but left out of [`Self::dismissed`], past
    /// [`MAX_DISMISS_REPORTED`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dismissed_omitted: usize,
    /// Refusals left out of [`Self::refused`], past [`MAX_DISMISS_REPORTED`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refused_omitted: usize,
    /// Set by a client that sent the dismissal in batches
    /// ([`dismiss_batches`]) when a batch after the first did not answer
    /// with a dismissal: the rest were not sent. The daemon never sets it;
    /// the desktop's bridge hands it to the webview with the outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<DismissStopped>,
}

/// Why a dismissal sent in batches stopped before its last batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DismissStopReason {
    /// The daemon process the batches were pinned to was replaced: a batch
    /// was withheld, or another process answered.
    Replaced,
    /// The daemon refused a batch whole; [`DismissStopped::refusal`] says why.
    Refused,
    /// A batch got no answer: the daemon could not be reached, or did not
    /// answer in time.
    Failed,
    #[serde(other)]
    Unknown,
}

/// A dismissal sent in batches that stopped before its last batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissStopped {
    pub reason: DismissStopReason,
    /// The refusal, for [`DismissStopReason::Refused`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<RefusalReason>,
    /// How many of the confirmed ids were not dismissed by it: the stopped
    /// batch's and every later one's.
    pub not_sent: usize,
}

/// The confirmed `ids` of a dismissal split into the selectors the daemon
/// admits, in order: each batch names at most
/// [`crate::close_agents::MAX_SELECTOR_ENTRIES`] ids, and at most
/// [`crate::close_agents::MAX_SELECTOR_BYTES`] of them together. An id longer
/// than [`crate::close_agents::MAX_SELECTOR_ENTRY_BYTES`] goes in a batch of
/// its own, which the daemon refuses. Every id is in exactly one batch.
pub fn dismiss_batches(ids: &[String]) -> Vec<Vec<String>> {
    use crate::close_agents::{MAX_SELECTOR_BYTES, MAX_SELECTOR_ENTRIES};
    let mut batches: Vec<Vec<String>> = Vec::new();
    let mut bytes = 0usize;
    for id in ids {
        let fits = batches.last().is_some_and(|batch| {
            batch.len() < MAX_SELECTOR_ENTRIES
                && bytes.saturating_add(id.len()) <= MAX_SELECTOR_BYTES
        });
        if !fits {
            batches.push(Vec::new());
            bytes = 0;
        }
        bytes = bytes.saturating_add(id.len());
        batches.last_mut().expect("pushed above").push(id.clone());
    }
    batches
}

/// The most entries each of a dismissal's [`DismissOutcome::dismissed`] and
/// [`DismissOutcome::refused`] carries; the rest are counted.
pub const MAX_DISMISS_REPORTED: usize = 256;

impl DismissOutcome {
    /// Add `batch`, the answer to a later batch of the same dismissal: its
    /// dismissed and refused entries, within [`MAX_DISMISS_REPORTED`] each,
    /// the rest counted, and its incompleteness. A whole-batch refusal is not
    /// added: the caller stops there and records it in [`Self::stopped`].
    pub fn absorb(&mut self, batch: DismissOutcome) {
        self.incomplete |= batch.incomplete;
        self.dismissed_omitted += batch.dismissed_omitted;
        self.refused_omitted += batch.refused_omitted;
        for item in batch.dismissed {
            self.push_dismissed(item);
        }
        for refused in batch.refused {
            if self.refused.len() < MAX_DISMISS_REPORTED {
                self.refused.push(refused);
            } else {
                self.refused_omitted += 1;
            }
        }
    }

    /// How many items it dismissed, named and counted.
    pub fn dismissed_count(&self) -> usize {
        self.dismissed.len() + self.dismissed_omitted
    }

    /// Record `item` as dismissed, within [`MAX_DISMISS_REPORTED`], its name
    /// cut to [`MAX_FIELD_BYTES`] as every display string a reply carries
    /// is (a launch record's name can be far longer).
    pub fn push_dismissed(&mut self, mut item: DismissedItem) {
        if self.dismissed.len() < MAX_DISMISS_REPORTED {
            if let Some(name) = item.name.as_mut() {
                cut_to(name, MAX_FIELD_BYTES);
            }
            self.dismissed.push(item);
        } else {
            self.dismissed_omitted += 1;
        }
    }

    /// Fit the answer into at most `budget` bytes of JSON: refusals are kept
    /// first, then dismissed items, each in order while the whole fits; the
    /// entries left out are counted in [`Self::refused_omitted`] and
    /// [`Self::dismissed_omitted`]. What was dismissed is already written;
    /// this bounds only what the answer names.
    pub fn fit(&mut self, budget: usize) {
        let (dismissed, refused) = (self.dismissed.len(), self.refused.len());
        // The envelope with every entry counted as omitted: the counts can
        // only be smaller once entries are kept.
        let mut used = json_len(&DismissOutcome {
            dismissed: Vec::new(),
            refused: Vec::new(),
            refusal: self.refusal.clone(),
            incomplete: self.incomplete,
            dismissed_omitted: self.dismissed_omitted + dismissed,
            refused_omitted: self.refused_omitted + refused,
            stopped: self.stopped.clone(),
        });
        let mut keep = |sizes: &mut dyn Iterator<Item = usize>| {
            let mut kept = 0;
            for size in sizes {
                // The entry and the comma before it.
                let size = size.saturating_add(1);
                if used.saturating_add(size) > budget {
                    break;
                }
                used += size;
                kept += 1;
            }
            kept
        };
        let kept_refused = keep(&mut self.refused.iter().map(json_len));
        let kept_dismissed = keep(&mut self.dismissed.iter().map(json_len));
        self.refused.truncate(kept_refused);
        self.refused_omitted += refused - kept_refused;
        self.dismissed.truncate(kept_dismissed);
        self.dismissed_omitted += dismissed - kept_dismissed;
    }

    /// Record a refusal of `id`, within [`MAX_DISMISS_REPORTED`].
    pub fn push_refused(&mut self, id: String, reason: RefusalReason) {
        if self.refused.len() < MAX_DISMISS_REPORTED {
            self.refused.push(DismissRefused {
                id,
                refusal: Refusal::new(reason),
            });
        } else {
            self.refused_omitted += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Decision D2: the words every client shows (CLAUDE.md rule 22). The CLI, the
// TUI and the desktop read them from here; the desktop's copies are pinned to
// these by a test.
// ---------------------------------------------------------------------------

/// The list's title.
pub const INTERRUPTED_WORK_TITLE: &str = "Interrupted work";
/// The group plain (non-dispatched) agents are listed in (decision D1).
pub const OTHER_AGENTS_GROUP: &str = "Other agents";
/// The action that removes an item's worktree and keeps its branch.
pub const RECLAIM_ACTION: &str = "Reclaim";
/// What [`RECLAIM_ACTION`] does.
pub const RECLAIM_EXPLANATION: &str = "remove its worktree, keep its branch";
/// The action that hides one item.
pub const DISMISS_ACTION: &str = "Dismiss";
/// The action that hides every item.
pub const DISMISS_ALL_ACTION: &str = "Dismiss all";
/// The words before a kept report's removal date (decision D7).
pub const KEPT_UNTIL: &str = "kept until";

/// The footer that says how many items there are: "N interrupted items from
/// before the restart", singular for one.
pub fn interrupted_footer(count: usize) -> String {
    if count == 1 {
        "1 interrupted item from before the restart".to_string()
    } else {
        format!("{count} interrupted items from before the restart")
    }
}

pub const RECLAIM_REMOVED: &str = "Reclaimed: its worktree was removed and its branch kept.";
pub const RECLAIM_WOULD_REMOVE: &str = "Reclaim would remove its worktree and keep its branch.";
pub const RECLAIM_KEPT_DIRTY: &str =
    "Kept: its worktree has uncommitted changes. Commit or discard them, then reclaim it again.";
pub const RECLAIM_KEPT_HOLDS_WORKTREES: &str =
    "Kept: other worktrees lie inside its worktree. Remove them first, then reclaim it again.";
pub const RECLAIM_KEPT_COULD_NOT_CHECK: &str =
    "Kept: whether its worktree has uncommitted changes could not be checked.";
pub const RECLAIM_REMOVE_FAILED: &str = "Kept: git could not remove its worktree.";
pub const RECLAIM_TIMED_OUT: &str = "Removing its worktree did not finish in time; it may still be removed, and the deck records it if it is.";
pub const RECLAIM_KEPT_CHANGED: &str = "Kept: its worktree was moved, replaced or switched to another branch while it was being reclaimed, so nothing was removed. List the interrupted work again.";
pub const RECLAIM_KEPT_BUSY: &str = "Kept: the deck could not get its repository's worktree lock in time, so nothing was removed. Reclaim it again.";
pub const RECLAIM_KEPT_COULD_NOT_LOCK: &str =
    "Kept: the deck could not take its repository's worktree lock, so nothing was removed.";
pub const RECLAIM_NOT_RECORDED: &str = "Its worktree was removed and its branch kept, but the deck could not record it, so it may be listed again.";
pub const DISMISSED: &str = "Dismissed.";

pub const REFUSED_AGENT_CALLER: &str = "Refused: this request came from an agent in a deck pane, and reclaiming or dismissing interrupted work is left to the person. Do it from the deck or from a terminal.";
pub const REFUSED_BAD_REQUEST: &str = "Refused: name the items to dismiss, or dismiss all of them.";
pub const REFUSED_TOO_LARGE: &str =
    "Refused: the request names too many items, or an id that is too long.";
pub const REFUSED_UNREADABLE: &str = "Refused: the deck could not read its work journal.";
pub const REFUSED_INCOMPLETE_READ: &str =
    "Refused: the deck could not read its whole work journal, so it cannot be sure of this item.";
pub const REFUSED_NOT_LISTED: &str = "No interrupted item has that id, name or branch.";
pub const REFUSED_AMBIGUOUS: &str = "More than one interrupted item matches; use the item's id.";
pub const REFUSED_PLAIN_AGENT: &str = "Refused: the deck did not create a worktree for this agent, so there is nothing to reclaim. Dismiss it instead.";
pub const REFUSED_NO_WORKTREE: &str = "Refused: its worktree is not on disk.";
pub const REFUSED_NOT_LINKED_WORKTREE: &str =
    "Refused: its directory is not a linked Git worktree.";
pub const REFUSED_BRANCH_MISMATCH: &str =
    "Refused: its worktree has another branch checked out than the one it was started on.";
pub const REFUSED_NOT_OWNED: &str = "Refused: the deck did not create this worktree.";
pub const REFUSED_NEWER_RUN: &str = "Refused: a later run used this worktree, so reclaiming this older item could remove that run's work. Dismiss this item instead.";
pub const REFUSED_OWNER_UNCONFIRMED: &str = "Refused: the deck cannot confirm this worktree is still this item's, for example because an older version of the deck created it. Dismiss this item instead, and remove the worktree with Git if you no longer need it.";
pub const REFUSED_STILL_IN_USE: &str = "Refused: an agent is still running in its worktree.";
pub const REFUSED_IN_USE_UNKNOWN: &str =
    "Refused: the deck could not tell whether an agent is still running in its worktree.";
pub const REFUSED_NOT_RECORDED: &str = "Refused: the deck could not record the dismissal.";
pub const REFUSED_UNKNOWN: &str = "Refused.";

/// What an empty list says.
pub const NOTHING_INTERRUPTED: &str = "No interrupted work.";
/// What a list (or a show) read from an incomplete journal read says.
pub const LIST_INCOMPLETE: &str =
    "The deck could not read its whole work journal, so an item may be missing or out of date.";
/// What the list says when asking for it again failed while an earlier
/// answer is still shown, before why it failed (PR #2431 round 4).
pub const LIST_REFRESH_FAILED: &str = "The list could not be refreshed, so it may be out of date.";
/// What the TUI's footer row and the desktop's banner say for a list that
/// came back empty but incomplete: there may be work the deck could not read,
/// so the row still offers a way into the list (verification finding N4).
pub const INTERRUPTED_UNREAD: &str =
    "The deck could not read its whole work journal, so some interrupted work may not be listed.";
/// The labels of an item's facts.
pub const LABEL_ISSUE: &str = "Issue";
pub const LABEL_BRANCH: &str = "Branch";
/// A dispatched unit's or an orchestration's directory.
pub const LABEL_WORKTREE: &str = "Worktree";
/// A plain agent's directory, which the deck did not create.
pub const LABEL_FOLDER: &str = "Folder";
pub const LABEL_STARTED_BY: &str = "Started by";
pub const LABEL_LAST_ENTRY: &str = "Last entry";
pub const LABEL_GIT: &str = "Git";
pub const LABEL_REPORT: &str = "Report";
pub const LABEL_PROGRESS: &str = "Progress";
/// A show asked for a report the item does not carry.
pub const REPORT_NOT_OF_ITEM: &str = "That report is not one of this item's kept reports.";
/// Before the item's strings when one was cut to [`MAX_FIELD_BYTES`].
pub const ITEM_CUT: &str = "Some of this item's text is too long to show in full.";
/// A worktree inside the one a reclaim kept (decision D6).
pub const LABEL_NESTED_WORKTREE: &str = "Holds worktree";
/// Before a report body that was cut at the show bound.
pub const REPORT_CUT: &str = "This report is longer than shown; this is its start.";
/// Before why a report body could not be shown.
pub const REPORT_UNREADABLE: &str = "This report cannot be shown:";
/// `dismiss --all` with no terminal to confirm on and no `--yes`.
pub const DISMISS_ALL_NEEDS_YES: &str = "Refused: there is no terminal to confirm on, so nothing was dismissed. Add --yes to dismiss every item.";
/// The person answered no to dismissing every item.
pub const NOTHING_DISMISSED: &str = "Nothing was dismissed.";
/// The running daemon does not advertise interrupted work.
pub const UNSUPPORTED: &str = "The running deck daemon does not support interrupted work. Stop it with `dot-agent-deck daemon restart`, then start one from this build: open the TUI, or press Start daemon in the desktop app.";
/// No daemon answered.
pub const NO_DAEMON: &str = "No deck daemon is running.";
/// Before the error of a request the daemon did not answer.
pub const DAEMON_FAILED: &str = "The deck daemon could not answer:";

/// The most characters of a value read from the journal or the daemon (a
/// name, a branch, a path, a daemon message) one line shows.
pub const MAX_SHOWN_CHARS: usize = 300;

/// `value` made safe to show on one terminal line, as the CLI and the TUI
/// show it: control characters (escapes, newlines) and Unicode bidi
/// controls are written escaped (`\u{1b}`, `\u{202e}`:
/// [`crate::untrusted_text::escape_control_and_bidi`]), so none can move the
/// cursor or reorder the text around it, and the result is cut to
/// [`MAX_SHOWN_CHARS`] characters, ending in `…` when it was.
pub fn display_value(value: &str) -> String {
    let escaped = crate::untrusted_text::escape_control_and_bidi(value);
    let mut chars = escaped.chars();
    let mut out: String = chars.by_ref().take(MAX_SHOWN_CHARS).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// A report body made safe to print as a block of lines: its newlines and
/// tabs kept as its layout (a `\r\n` read as a newline), and every other
/// control character and the bidi controls escaped as [`display_value`]
/// escapes them; not cut. The daemon frames a body before sending it, and
/// this does not trust that it did.
pub fn display_block(body: &str) -> String {
    body.replace("\r\n", "\n")
        .split('\n')
        .map(|line| {
            line.split('\t')
                .map(crate::untrusted_text::escape_control_and_bidi)
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The word for an item's kind.
pub fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Unit => "Unit",
        ItemKind::Orchestration => "Orchestration",
        ItemKind::IssueDispatch => "Issue dispatch",
        ItemKind::Agent => "Agent",
    }
}

/// A time in milliseconds since the Unix epoch, as `YYYY-MM-DD HH:MM UTC`.
pub fn format_time(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "an unknown time".to_string())
}

/// "kept until <date>" for a kept report (decision D7), or what is said
/// while it is held with no removal date yet.
pub fn kept_until(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => format!("{KEPT_UNTIL} {}", format_time(ms)),
        None => "kept, with no removal date yet".to_string(),
    }
}

/// The question `dismiss --all` asks before it dismisses `count` items.
pub fn dismiss_all_question(count: usize) -> String {
    format!(
        "{DISMISS_ALL_ACTION}: {}? [y/N] ",
        interrupted_footer(count)
    )
}

/// What an item or a show that left out `count` of its kept reports says,
/// and how to read one: `dot-agent-deck interrupted show <id> --report
/// <report id>`.
pub fn more_reports_sentence(count: usize, id: &str) -> String {
    let lead = if count == 1 {
        "1 more report is kept".to_string()
    } else {
        format!("{count} more reports are kept")
    };
    format!("{lead}; read one with `dot-agent-deck interrupted show {id} --report <report id>`.")
}

/// How to list an item's kept reports from the `next`-th on: `dot-agent-deck
/// interrupted show <id> --reports-from <next>`.
pub fn next_reports_sentence(id: &str, next: usize) -> String {
    format!("List the next ones with `dot-agent-deck interrupted show {id} --reports-from {next}`.")
}

/// Which of an item's `total` kept reports a page holds, counted from one.
pub fn report_range_sentence(first: usize, last: usize, total: usize) -> String {
    if first == last {
        format!("Report {first} of {total}.")
    } else {
        format!("Reports {first} to {last} of {total}.")
    }
}

/// What a dismissal that dismissed `count` items more than it named says.
pub fn more_dismissed_sentence(count: usize) -> String {
    if count == 1 {
        "1 more item was dismissed.".to_string()
    } else {
        format!("{count} more items were dismissed.")
    }
}

/// What a dismissal that refused `count` ids more than it named says.
pub fn more_refused_sentence(count: usize) -> String {
    if count == 1 {
        "1 more id was refused.".to_string()
    } else {
        format!("{count} more ids were refused.")
    }
}

/// Why a dismissal sent in batches stopped, when its daemon was replaced.
pub const DISMISS_STOPPED_REPLACED: &str =
    "the deck daemon was replaced. List the interrupted work again.";
/// Why a dismissal sent in batches stopped, when a batch got no answer.
pub const DISMISS_STOPPED_FAILED: &str =
    "the deck daemon did not answer. List the interrupted work again.";

/// What a dismissal sent in batches says after its outcome when it stopped
/// before its last batch ([`DismissOutcome::stopped`]). A refused batch was
/// not dismissed; a batch whose daemon was replaced, or that got no answer,
/// may have been, so that is all it says of them.
pub fn dismiss_stopped_sentence(stopped: &DismissStopped) -> String {
    let n = stopped.not_sent;
    let (one, many) = match stopped.reason {
        DismissStopReason::Refused | DismissStopReason::Unknown => (
            "1 more item was not dismissed",
            "more items were not dismissed",
        ),
        DismissStopReason::Replaced | DismissStopReason::Failed => (
            "1 more item may not have been dismissed",
            "more items may not have been dismissed",
        ),
    };
    let lead = if n == 1 {
        one.to_string()
    } else {
        format!("{n} {many}")
    };
    match stopped.reason {
        DismissStopReason::Replaced => format!("{lead}: {DISMISS_STOPPED_REPLACED}"),
        DismissStopReason::Failed => format!("{lead}: {DISMISS_STOPPED_FAILED}"),
        DismissStopReason::Refused => format!(
            "{lead}. {}",
            refusal_sentence(stopped.refusal.unwrap_or(RefusalReason::Unknown))
        ),
        DismissStopReason::Unknown => format!("{lead}."),
    }
}

/// What a show that left out the bodies of `count` reports says.
pub fn bodies_omitted_sentence(count: usize, id: &str) -> String {
    let lead = if count == 1 {
        "1 report is not shown here, to keep the answer small".to_string()
    } else {
        format!("{count} reports are not shown here, to keep the answer small")
    };
    format!("{lead}; read one with `dot-agent-deck interrupted show {id} --report <report id>`.")
}

/// An item's progress on one line: its attempts, the roles they ran as, and
/// its delegations done of those opened.
pub fn progress_line(progress: &Progress) -> String {
    let mut parts = vec![if progress.attempts == 1 {
        "1 attempt".to_string()
    } else {
        format!("{} attempts", progress.attempts)
    }];
    if !progress.roles.is_empty() {
        parts.push(format!("roles {}", progress.roles.join(", ")));
    }
    if progress.delegations > 0 {
        let noun = if progress.delegations == 1 {
            "delegation"
        } else {
            "delegations"
        };
        parts.push(format!(
            "{} of {} {noun} done",
            progress.delegations_done, progress.delegations
        ));
    }
    parts.join("; ")
}

/// What a list that left out its oldest `count` items says.
pub fn omitted_sentence(count: usize) -> String {
    if count == 1 {
        "1 older item is not shown.".to_string()
    } else {
        format!("{count} older items are not shown.")
    }
}

/// An item's Git and pull request facts on one line; a fact that could not
/// be read is left out.
pub fn git_facts_line(git: &GitFacts) -> String {
    let mut facts: Vec<String> = Vec::new();
    match git.worktree_exists {
        Some(true) => facts.push("worktree on disk".into()),
        Some(false) => facts.push("worktree gone".into()),
        None => {}
    }
    match git.dirty {
        Some(true) => facts.push("uncommitted changes".into()),
        Some(false) => facts.push("no uncommitted changes".into()),
        None => {}
    }
    match git.unpushed_commits {
        Some(1) => facts.push("1 commit on no remote".into()),
        Some(n) if n > 1 => facts.push(format!("{n} commits on no remote")),
        _ => {}
    }
    match git.branch_exists {
        Some(true) => facts.push("branch kept".into()),
        Some(false) => facts.push("branch gone".into()),
        None => {}
    }
    facts.push(
        match git.pr {
            PrFact::Open => "pull request open",
            PrFact::Merged => "pull request merged",
            PrFact::Closed => "pull request closed",
            PrFact::None => "no pull request",
            PrFact::Unknown => "pull request unknown",
        }
        .into(),
    );
    facts.join(", ")
}

/// The sentence for a reclaim verdict that is not a refusal.
pub fn reclaim_sentence(verdict: ReclaimVerdict) -> &'static str {
    match verdict {
        ReclaimVerdict::Removed => RECLAIM_REMOVED,
        ReclaimVerdict::WouldRemove => RECLAIM_WOULD_REMOVE,
        ReclaimVerdict::KeptDirty => RECLAIM_KEPT_DIRTY,
        ReclaimVerdict::KeptHoldsWorktrees => RECLAIM_KEPT_HOLDS_WORKTREES,
        ReclaimVerdict::KeptCouldNotCheck => RECLAIM_KEPT_COULD_NOT_CHECK,
        ReclaimVerdict::RemoveFailed => RECLAIM_REMOVE_FAILED,
        ReclaimVerdict::TimedOut => RECLAIM_TIMED_OUT,
        ReclaimVerdict::KeptChanged => RECLAIM_KEPT_CHANGED,
        ReclaimVerdict::KeptBusy => RECLAIM_KEPT_BUSY,
        ReclaimVerdict::KeptCouldNotLock => RECLAIM_KEPT_COULD_NOT_LOCK,
        ReclaimVerdict::Refused | ReclaimVerdict::Unknown => REFUSED_UNKNOWN,
    }
}

/// The sentence for a refusal.
pub fn refusal_sentence(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::AgentCaller => REFUSED_AGENT_CALLER,
        RefusalReason::BadRequest => REFUSED_BAD_REQUEST,
        RefusalReason::TooLarge => REFUSED_TOO_LARGE,
        RefusalReason::Unreadable => REFUSED_UNREADABLE,
        RefusalReason::IncompleteRead => REFUSED_INCOMPLETE_READ,
        RefusalReason::NotListed => REFUSED_NOT_LISTED,
        RefusalReason::Ambiguous => REFUSED_AMBIGUOUS,
        RefusalReason::PlainAgent => REFUSED_PLAIN_AGENT,
        RefusalReason::NoWorktree => REFUSED_NO_WORKTREE,
        RefusalReason::NotLinkedWorktree => REFUSED_NOT_LINKED_WORKTREE,
        RefusalReason::BranchMismatch => REFUSED_BRANCH_MISMATCH,
        RefusalReason::NotOwned => REFUSED_NOT_OWNED,
        RefusalReason::NewerRun => REFUSED_NEWER_RUN,
        RefusalReason::OwnerUnconfirmed => REFUSED_OWNER_UNCONFIRMED,
        RefusalReason::StillInUse => REFUSED_STILL_IN_USE,
        RefusalReason::InUseUnknown => REFUSED_IN_USE_UNKNOWN,
        RefusalReason::NotRecorded => REFUSED_NOT_RECORDED,
        RefusalReason::Unknown => REFUSED_UNKNOWN,
    }
}

/// The sentence for unpushed commits a reclaim found (decision D5), or
/// `None` when there are none.
pub fn unpushed_sentence(count: u64) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 commit on its branch is on no remote; the branch keeps it.".to_string()),
        n => Some(format!(
            "{n} commits on its branch are on no remote; the branch keeps them."
        )),
    }
}

/// What a request's id resolved to in a list.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved<'a> {
    One(&'a InterruptedItem),
    NotListed,
    Ambiguous,
}

/// The listed item `query` names: its id exactly, else the one item whose
/// name or branch equals it, or whose id ends with it.
///
/// A query that parses as a full work id ([`WorkId::parse`]) names that
/// item or none (auditor N1): when the item it named is no longer listed
/// (another client dismissed or reclaimed it), an item whose name or branch
/// happens to be that string is not taken in its place, so a confirmed or
/// batched action cannot land on an item the person did not confirm.
pub fn resolve<'a>(list: &'a InterruptedList, query: &str) -> Resolved<'a> {
    let query = query.trim();
    if query.is_empty() {
        return Resolved::NotListed;
    }
    let exact = list.items.iter().find(|item| item.id.as_str() == query);
    if WorkId::parse(query).is_some() {
        return exact.map_or(Resolved::NotListed, Resolved::One);
    }
    if let Some(item) = exact {
        return Resolved::One(item);
    }
    let mut matches = list.items.iter().filter(|item| {
        item.name.as_deref() == Some(query)
            || item.branch.as_deref() == Some(query)
            || item.id.as_str().ends_with(query)
    });
    match (matches.next(), matches.next()) {
        (Some(item), None) => Resolved::One(item),
        (Some(_), Some(_)) => Resolved::Ambiguous,
        (None, _) => Resolved::NotListed,
    }
}

/// Whether the item rooted at `root` is still interrupted in `read`: a work
/// under it is interrupted, none has a live attempt, and the root was not
/// dismissed or reclaimed. An item listed only for a report left for a
/// caller that is gone (decision D3) is not.
pub fn still_interrupted(
    read: &JournalRead,
    root: &WorkId,
    is_live: impl Fn(&str) -> bool,
) -> bool {
    let no_records = LaunchRead::default();
    let index = Index::build(read, &no_records, is_live);
    let roots = index.roots();
    let Some(works) = roots.under.get(root) else {
        return false;
    };
    !index.closed_by_person(root)
        && works.iter().any(|w| index.interrupted(w))
        && !works.iter().any(|w| index.has_live_attempt(w))
}

/// PRD #1790 stage 3: the action that opens the restore question.
pub const RESTORE_ACTION: &str = "Restore";
/// The question's choice that restores the agent and lets it carry on.
pub const RESTORE_AND_CONTINUE: &str = "Restore and continue";
/// The question's choice that restores the agent and lets it wait.
pub const RESTORE_ONLY: &str = "Restore only";
/// The question's choice for an agent that cannot be resumed.
pub const START_OVER: &str = "Start over in this worktree";
/// What [`RESTORE_AND_CONTINUE`] does, under the question.
pub const RESTORE_AND_CONTINUE_EXPLANATION: &str =
    "It checks what already happened and carries on where it stopped.";
/// What [`RESTORE_ONLY`] does, under the question.
pub const RESTORE_ONLY_EXPLANATION: &str = "It comes back and waits until you tell it to continue.";

/// The restore question for the agent or item called `name`.
pub fn restore_question(name: &str) -> String {
    format!("Restore {name} with its conversation?")
}

/// The note a restored agent receives first after [`RESTORE_AND_CONTINUE`].
pub const RESTART_NOTE_CONTINUE: &str = "The deck restarted and resumed this session. If your previous turn was cut off before it finished, first check the current state — `git status`, `git log`, whether anything was already pushed, opened or sent — then continue from where you stopped without repeating what already happened. If your previous turn had finished, do nothing and wait for the next message.";
/// The note a restored agent receives first after [`RESTORE_ONLY`].
pub const RESTART_NOTE_ONLY: &str = "The deck restarted and resumed this session. Your previous turn may have been cut off before it finished. Do nothing until you are told to continue; when you are, first check the current state — `git status`, `git log`, whether anything was already pushed, opened or sent — and do not repeat what already happened.";

/// Why an agent is not offered a resume, in the words every client shows.
pub fn restore_reason_sentence(reason: RestoreReason) -> &'static str {
    match reason {
        RestoreReason::AgentCannotResume => {
            "The agent cannot be restored with its conversation yet."
        }
        RestoreReason::NoSessionId => "The deck does not know which conversation the agent was in.",
        RestoreReason::SessionIdInvalid => {
            "The conversation id the deck holds for the agent is not one it can resume."
        }
        RestoreReason::NoLaunchRecord => "The deck has no record of how the agent was started.",
        RestoreReason::AmbiguousRecord => {
            "The deck's records do not say for certain how the agent was started."
        }
        RestoreReason::FolderMoved => {
            "The agent's folder was moved, is gone, or others can write to it."
        }
        RestoreReason::FolderUnknown => "The deck does not know which folder the agent ran in.",
        RestoreReason::OrchestrationLater => "Orchestrations come back in a later version.",
        RestoreReason::ClearWorker => {
            "This worker starts fresh for each task and comes back with its orchestration."
        }
        RestoreReason::DefaultShell => {
            "The pane ran a shell, not an agent, so there is no conversation to restore."
        }
        RestoreReason::IncompleteRead => "The deck could not read its whole work journal.",
        RestoreReason::NotInterrupted => "The restart did not stop this agent.",
        RestoreReason::Unknown => "The agent cannot be restored.",
    }
}

/// The list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedList {
    /// Most recent last entry first.
    pub items: Vec<InterruptedItem>,
    /// The journal read or the launch-record read stopped at its budget, so
    /// an item may be missing, or listed although an end or close that would
    /// hide it was not read.
    #[serde(default)]
    pub incomplete: bool,
    /// Items left out past [`MAX_LISTED_ITEMS`], the oldest ones.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The list, from a journal read, a launch-record read, whether each run is
/// live, and the kept report files. Workspaces under `~` are expanded
/// against the current user's home directory.
pub fn derive(
    read: &JournalRead,
    launches: &LaunchRead,
    is_live: impl Fn(&str) -> bool,
    kept_reports: &[KeptReport],
) -> InterruptedList {
    derive_with_home(
        read,
        launches,
        is_live,
        kept_reports,
        &crate::platform::paths::home_dir(),
    )
}

/// [`derive`] against an explicit home directory.
pub fn derive_with_home(
    read: &JournalRead,
    launches: &LaunchRead,
    is_live: impl Fn(&str) -> bool,
    kept_reports: &[KeptReport],
    home: &Path,
) -> InterruptedList {
    let index = Index::build(read, launches, is_live);
    let roots = index.roots();

    // Reports left for a caller that is gone, by the root of their producer.
    let mut kept_by_id: HashMap<&str, Vec<&KeptReport>> = HashMap::new();
    for kept in kept_reports {
        kept_by_id.entry(&kept.report_id).or_default().push(kept);
    }
    let mut reports_of: HashMap<&WorkId, Vec<ItemReport>> = HashMap::new();
    for reported in &index.reported {
        let Some(kept) = kept_by_id
            .get(reported.report_id.as_str())
            .and_then(|kept| {
                kept.iter().find(|kept| {
                    !kept.acknowledged()
                        && (kept.left_for_gone_caller()
                            || reported.destination == Destination::Recovery)
                })
            })
        else {
            continue;
        };
        let Some(root) = roots.root.get(reported.work) else {
            continue;
        };
        let list = reports_of.entry(*root).or_default();
        if list.iter().all(|r| &r.report_id != reported.report_id) {
            list.push(ItemReport {
                report_id: reported.report_id.clone(),
                task_id: reported.task_id.clone(),
                unit_id: reported.unit_id.clone(),
                kept_until_ms: kept.kept_until_ms,
            });
        }
    }

    // A launch-record read that skipped a record may have skipped the one
    // that would make an agent's record ambiguous.
    let incomplete = read.stats.truncated || launches.truncated || launches.skipped > 0;
    let mut items = Vec::new();
    for (root, works) in &roots.under {
        if index.closed_by_person(root) {
            continue;
        }
        let interrupted = works.iter().any(|w| index.interrupted(w))
            && !works.iter().any(|w| index.has_live_attempt(w));
        let reports = reports_of.remove(root).unwrap_or_default();
        if !interrupted && reports.is_empty() {
            continue;
        }
        items.push(index.item(
            root,
            works,
            roots.launcher.get(root),
            reports,
            home,
            incomplete,
        ));
    }
    items.sort_by(|a, b| {
        b.last_entry
            .ts_ms
            .cmp(&a.last_entry.ts_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    InterruptedList {
        items,
        incomplete,
        omitted: 0,
    }
}

/// For every root of one read, the works a dismissal or reclaim of its item
/// closes. Built once per read ([`CloseSets::build`]) and asked per item, so
/// dismissing many items indexes the journal once, not once per item.
#[derive(Debug, Default)]
pub struct CloseSets {
    /// Each root's works with no `closed` line, the root excluded.
    unclosed_under: HashMap<WorkId, Vec<WorkId>>,
}

impl CloseSets {
    /// Index `read` once.
    pub fn build(read: &JournalRead) -> Self {
        let no_records = LaunchRead::default();
        let index = Index::build(read, &no_records, |_| true);
        let roots = index.roots();
        let unclosed_under = roots
            .under
            .iter()
            .map(|(root, works)| {
                let unclosed = works
                    .iter()
                    .filter(|w| *w != root && !index.closes.contains_key(*w))
                    .map(|w| (*w).clone())
                    .collect();
                ((*root).clone(), unclosed)
            })
            .collect();
        CloseSets { unclosed_under }
    }

    /// The works a dismissal or reclaim of the item rooted at `root` closes:
    /// the root itself, closed or not, so the item is hidden, and every work
    /// that rolls up to it and has no `closed` line yet, so none of them is
    /// listed on its own later. The root comes first.
    pub fn works_under(&self, root: &WorkId) -> Vec<WorkId> {
        let mut out = vec![root.clone()];
        if let Some(works) = self.unclosed_under.get(root) {
            out.extend(works.iter().cloned());
        }
        out
    }
}

/// [`CloseSets::works_under`] for one root, indexing `read` for it alone.
pub fn works_under(read: &JournalRead, root: &WorkId) -> Vec<WorkId> {
    CloseSets::build(read).works_under(root)
}

/// `value` cut to at most `max` bytes on a character boundary; `true` when
/// it was cut.
fn cut_to(value: &mut String, max: usize) -> bool {
    if value.len() <= max {
        return false;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

/// Cut every display string of `item` to [`MAX_FIELD_BYTES`], marking it
/// [`InterruptedItem::cut`]. Report ids are never cut: a show selects by
/// them.
fn cut_fields(item: &mut InterruptedItem) {
    let mut cut = false;
    for field in [
        &mut item.name,
        &mut item.repo,
        &mut item.branch,
        &mut item.worktree,
        &mut item.launcher,
    ]
    .into_iter()
    .flatten()
    {
        cut |= cut_to(field, MAX_FIELD_BYTES);
    }
    cut |= cut_to(&mut item.last_entry.kind, MAX_FIELD_BYTES);
    for role in &mut item.progress.roles {
        cut |= cut_to(role, MAX_FIELD_BYTES);
    }
    for agent in &mut item.agents {
        for field in [&mut agent.name, &mut agent.role, &mut agent.folder]
            .into_iter()
            .flatten()
        {
            cut |= cut_to(field, MAX_FIELD_BYTES);
        }
    }
    for report in &mut item.reports {
        for field in [&mut report.task_id, &mut report.unit_id]
            .into_iter()
            .flatten()
        {
            cut |= cut_to(field, MAX_FIELD_BYTES);
        }
    }
    item.cut |= cut;
}

/// The bytes `value` takes as JSON. A value that cannot be serialised counts
/// as too large for any budget.
pub fn json_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// Fit a list into a reply of at most `budget` bytes, setting aside
/// `reserve_per_item` bytes for each item kept (the Git facts a list adds
/// afterwards): each item carries at most [`MAX_LISTED_REPORTS`] report
/// entries, each string at most [`MAX_FIELD_BYTES`], and items are kept,
/// newest first, while the whole list fits and at most [`MAX_LISTED_ITEMS`]
/// are kept. [`InterruptedList::omitted`] counts the items left out. The
/// list's own items are cut in place, so what a reclaim or a dismissal acts
/// on must be resolved from an unfitted list.
pub fn fit_list(list: &mut InterruptedList, budget: usize, reserve_per_item: usize) {
    let total = list.items.len();
    let mut used = json_len(&InterruptedList {
        items: Vec::new(),
        incomplete: list.incomplete,
        omitted: total,
    });
    let mut kept = 0;
    for item in list.items.iter_mut() {
        if kept == MAX_LISTED_ITEMS {
            break;
        }
        if item.reports.len() > MAX_LISTED_REPORTS {
            item.reports_omitted += item.reports.len() - MAX_LISTED_REPORTS;
            item.reports.truncate(MAX_LISTED_REPORTS);
        }
        cut_fields(item);
        // The item and the comma before it.
        let size = json_len(item)
            .saturating_add(reserve_per_item)
            .saturating_add(1);
        if used.saturating_add(size) > budget {
            break;
        }
        used += size;
        kept += 1;
    }
    list.items.truncate(kept);
    list.omitted += total - kept;
}

/// Fit a show of `item` into a reply of at most `budget` bytes: the first
/// page of its kept reports' entries, as many as take at most
/// [`MAX_SHOWN_ENTRY_BYTES`] ([`fit_show_page`] with no page asked for).
pub fn fit_show(
    item: InterruptedItem,
    bodies: &[String],
    incomplete: bool,
    budget: usize,
    read: impl FnMut(&str) -> ShownReport,
) -> InterruptedShow {
    fit_show_page(item, None, bodies, incomplete, budget, read)
}

/// Fit a show of `item`, `item` as derived (every kept report listed), into
/// a reply of at most `budget` bytes. The item's strings are cut to
/// [`MAX_FIELD_BYTES`]. Its report entries are kept from the `page`-th
/// (0 when no page is asked for), in order, while they take at most
/// [`MAX_SHOWN_ENTRY_BYTES`] and, for a page asked for, at most
/// [`MAX_SHOWN_REPORTS`] of them; [`InterruptedItem::reports_omitted`]
/// counts every other report of the item, those before the page included.
/// [`InterruptedShow::reports_from`] is where the entries start and
/// [`InterruptedShow::next_reports_from`] where the next page would, so
/// every kept report can be found a page at a time (reviewer 1). Then the
/// bodies `read` answers for `bodies` (report ids) are added, in order,
/// while the whole show fits ([`InterruptedShow::bodies_omitted`] counts the
/// rest).
pub fn fit_show_page(
    mut item: InterruptedItem,
    page: Option<usize>,
    bodies: &[String],
    incomplete: bool,
    budget: usize,
    mut read: impl FnMut(&str) -> ShownReport,
) -> InterruptedShow {
    cut_fields(&mut item);
    let available = item.reports.len();
    let total = available + item.reports_omitted;
    let from = page.unwrap_or(0).min(available);
    item.reports.drain(..from);
    let most = if page.is_some() {
        MAX_SHOWN_REPORTS
    } else {
        usize::MAX
    };
    let mut entries = 0usize;
    let mut kept = 0usize;
    for report in &item.reports {
        let size = json_len(report).saturating_add(1);
        // At least one entry, so a page always moves on.
        if kept == most || (kept > 0 && entries.saturating_add(size) > MAX_SHOWN_ENTRY_BYTES) {
            break;
        }
        entries += size;
        kept += 1;
    }
    item.reports.truncate(kept);
    item.reports_omitted = total - kept;
    let next = from + kept;
    let mut show = InterruptedShow {
        item,
        reports: Vec::new(),
        incomplete,
        bodies_omitted: 0,
        reports_from: from,
        next_reports_from: (next < available).then_some(next),
    };
    let mut used = json_len(&InterruptedShow {
        bodies_omitted: bodies.len(),
        ..show.clone()
    });
    for (at, report_id) in bodies.iter().enumerate() {
        let shown = read(report_id);
        let size = json_len(&shown).saturating_add(1);
        if used.saturating_add(size) > budget {
            show.bodies_omitted = bodies.len() - at;
            break;
        }
        used += size;
        show.reports.push(shown);
    }
    show
}

/// The resolved directory of `workspace`: the first of `cwds` (absolute
/// paths recorded for the same place, such as launch records' `cwd`) that
/// redacts to it, else the workspace expanded against `home`.
pub fn workspace_path<'a>(
    workspace: &Workspace,
    cwds: impl IntoIterator<Item = &'a str>,
    home: &Path,
) -> Option<String> {
    for cwd in cwds {
        if workspace.matches_path_with_home(Path::new(cwd), home) {
            return Some(cwd.to_string());
        }
    }
    workspace
        .to_path_with_home(home)
        .and_then(|path| path.to_str().map(str::to_string))
}

/// A work's `opened` line.
struct Opened<'a> {
    /// The line's index in the read.
    at: usize,
    launch: Launch,
    parent: Option<&'a WorkId>,
    refs: &'a Refs,
    workspace: Option<&'a Workspace>,
}

/// A `reported` line.
struct Reported<'a> {
    work: &'a WorkId,
    report_id: &'a String,
    task_id: &'a Option<String>,
    unit_id: &'a Option<String>,
    destination: Destination,
}

/// How an attempt stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttemptState {
    Ended { interrupted: bool },
    Open { live: bool },
}

/// One pass over a read, keyed by work and attempt id.
struct Index<'a> {
    /// Every work a line names, in the order first seen.
    works: Vec<&'a WorkId>,
    opened: HashMap<&'a WorkId, Opened<'a>>,
    attempts_of: HashMap<&'a WorkId, Vec<&'a AttemptId>>,
    /// The roles each work's attempts ran as, each with the index in the
    /// read of the line that first named it for that work.
    roles_of: HashMap<&'a WorkId, Vec<(usize, &'a String)>>,
    attempt_state: HashMap<&'a AttemptId, AttemptState>,
    closes: HashMap<&'a WorkId, Vec<CloseHow>>,
    reported: Vec<Reported<'a>>,
    /// The index in the read of each work's last line.
    last_line: HashMap<&'a WorkId, usize>,
    lines: &'a JournalRead,
    /// Launch records joined to the journal, by attempt.
    records: HashMap<&'a AttemptId, &'a LaunchRecord>,
    /// Attempts with a launch record that could not be joined: more than one
    /// record, or a record whose work the journal does not hold the attempt
    /// under alone.
    ambiguous: HashSet<&'a AttemptId>,
    /// The role each attempt's first `attempt_started` names.
    role_of: HashMap<&'a AttemptId, &'a String>,
}

/// Each work's root, each root's works and each root's launcher work.
struct Roots<'a> {
    root: HashMap<&'a WorkId, &'a WorkId>,
    under: std::collections::BTreeMap<&'a WorkId, Vec<&'a WorkId>>,
    launcher: HashMap<&'a WorkId, &'a WorkId>,
}

impl<'a> Index<'a> {
    fn build(
        read: &'a JournalRead,
        launches: &'a LaunchRead,
        is_live: impl Fn(&str) -> bool,
    ) -> Self {
        let mut index = Index {
            works: Vec::new(),
            opened: HashMap::new(),
            attempts_of: HashMap::new(),
            roles_of: HashMap::new(),
            attempt_state: HashMap::new(),
            closes: HashMap::new(),
            reported: Vec::new(),
            last_line: HashMap::new(),
            lines: read,
            records: HashMap::new(),
            ambiguous: HashSet::new(),
            role_of: HashMap::new(),
        };
        let mut seen: HashSet<&WorkId> = HashSet::new();
        let mut ends: HashMap<&AttemptId, bool> = HashMap::new();
        // Which works the journal holds each attempt under.
        let mut works_of: HashMap<&AttemptId, BTreeSet<&WorkId>> = HashMap::new();
        for (at, line) in read.lines.iter().enumerate() {
            let work = line.event.work();
            if seen.insert(work) {
                index.works.push(work);
            }
            index.last_line.insert(work, at);
            match &line.event {
                Event::Opened {
                    launch,
                    parent,
                    refs,
                    workspace,
                    ..
                } => {
                    index.opened.entry(work).or_insert(Opened {
                        at,
                        launch: *launch,
                        parent: parent.as_ref(),
                        refs,
                        workspace: workspace.as_ref(),
                    });
                }
                Event::AttemptStarted { attempt, role, .. } => {
                    let list = index.attempts_of.entry(work).or_default();
                    if !list.contains(&attempt) {
                        list.push(attempt);
                    }
                    works_of.entry(attempt).or_default().insert(work);
                    if let Some(role) = role {
                        index.role_of.entry(attempt).or_insert(role);
                        let roles = index.roles_of.entry(work).or_default();
                        if !roles.iter().any(|(_, seen)| *seen == role) {
                            roles.push((at, role));
                        }
                    }
                }
                Event::AttemptEnded { attempt, how, .. } => {
                    let interrupted = ends.entry(attempt).or_insert(false);
                    *interrupted |= *how == EndHow::Interrupted;
                }
                Event::Reported {
                    report_id,
                    task_id,
                    unit_id,
                    destination,
                    ..
                } => index.reported.push(Reported {
                    work,
                    report_id,
                    task_id,
                    unit_id,
                    destination: *destination,
                }),
                Event::Closed { how, .. } => index.closes.entry(work).or_default().push(*how),
                Event::AttemptSession { .. } | Event::Linked { .. } => {}
            }
        }
        let mut live_runs: HashMap<&str, bool> = HashMap::new();
        for attempts in index.attempts_of.values() {
            for attempt in attempts {
                let state = match ends.get(attempt) {
                    Some(interrupted) => AttemptState::Ended {
                        interrupted: *interrupted,
                    },
                    None => {
                        let run = attempt.run();
                        let live = *live_runs.entry(run).or_insert_with(|| is_live(run));
                        AttemptState::Open { live }
                    }
                };
                index.attempt_state.insert(attempt, state);
            }
        }
        index.join_records(launches, &works_of);
        index
    }

    /// Join launch records by attempt. A record is used only when it is the
    /// one record for its attempt and the journal holds that attempt under
    /// the record's work and under no other work but delegations (whose
    /// `attempt_started` names the worker generation of their role by
    /// design). Anything else is ambiguous and ignored.
    fn join_records(
        &mut self,
        launches: &'a LaunchRead,
        works_of: &HashMap<&'a AttemptId, BTreeSet<&'a WorkId>>,
    ) {
        let mut by_attempt: HashMap<&AttemptId, Vec<&LaunchRecord>> = HashMap::new();
        for record in &launches.records {
            by_attempt.entry(&record.attempt).or_default().push(record);
        }
        for (attempt, records) in by_attempt {
            let Some((journal_attempt, works)) = works_of.get_key_value(attempt) else {
                continue;
            };
            let [record] = records.as_slice() else {
                self.ambiguous.insert(*journal_attempt);
                continue;
            };
            let holds = works.contains(&&record.work);
            let others_are_delegations = works.iter().all(|w| {
                *w == &record.work
                    || self
                        .opened
                        .get(w)
                        .is_some_and(|o| o.launch == Launch::Delegation)
            });
            if holds && others_are_delegations {
                self.records.insert(*journal_attempt, *record);
            } else {
                self.ambiguous.insert(*journal_attempt);
            }
        }
    }

    fn roots(&self) -> Roots<'a> {
        let mut roots = Roots {
            root: HashMap::new(),
            under: std::collections::BTreeMap::new(),
            launcher: HashMap::new(),
        };
        for work in &self.works {
            let (root, launcher) = self.walk(work);
            roots.root.insert(work, root);
            roots.under.entry(root).or_default().push(work);
            if let Some(launcher) = launcher {
                roots.launcher.insert(root, launcher);
            }
        }
        roots
    }

    /// `work`'s root and the work that launched the root, if any.
    fn walk(&self, work: &'a WorkId) -> (&'a WorkId, Option<&'a WorkId>) {
        let mut current = work;
        let mut visited: HashSet<&WorkId> = HashSet::from([work]);
        for _ in 0..MAX_WALK {
            let Some(opened) = self.opened.get(current) else {
                return (current, None);
            };
            let Some(parent) = opened.parent else {
                return (current, None);
            };
            let parent_launch = self.opened.get(parent).map(|o| o.launch);
            let continues = matches!(
                (opened.launch, parent_launch),
                (Launch::Delegation, _)
                    | (
                        Launch::Orchestration,
                        Some(Launch::Dispatch | Launch::IssueDispatch)
                    )
            );
            if !continues || !visited.insert(parent) {
                return (current, Some(parent));
            }
            current = parent;
        }
        (current, None)
    }

    fn closed_by_person(&self, work: &WorkId) -> bool {
        self.closes.get(work).is_some_and(|hows| {
            hows.iter()
                .any(|how| matches!(how, CloseHow::Dismissed | CloseHow::Reclaimed))
        })
    }

    fn interrupted(&self, work: &WorkId) -> bool {
        !self.closes.contains_key(work)
            && self.attempts(work).any(|attempt| {
                matches!(
                    self.attempt_state.get(attempt),
                    Some(
                        AttemptState::Ended { interrupted: true }
                            | AttemptState::Open { live: false }
                    )
                )
            })
    }

    fn has_live_attempt(&self, work: &WorkId) -> bool {
        self.attempts(work).any(|attempt| {
            matches!(
                self.attempt_state.get(attempt),
                Some(AttemptState::Open { live: true })
            )
        })
    }

    fn attempts(&self, work: &WorkId) -> impl Iterator<Item = &'a AttemptId> + '_ {
        self.attempts_of.get(work).into_iter().flatten().copied()
    }

    /// How far the item made of `ordered` (its root first) got. Its roles
    /// are ordered by where the journal first names each, across all of the
    /// item's works: an attempt's `attempt_started`, or a delegation's
    /// `opened` naming the role it was delegated to.
    fn progress(&self, ordered: &[&'a WorkId]) -> Progress {
        let mut attempts: HashSet<&AttemptId> = HashSet::new();
        let mut named: Vec<(usize, &String)> = Vec::new();
        let mut progress = Progress::default();
        for work in ordered {
            attempts.extend(self.attempts(work));
            let opened = self.opened.get(*work);
            let delegated_role = opened
                .filter(|o| o.launch == Launch::Delegation)
                .and_then(|o| o.refs.role.as_ref().map(|role| (o.at, role)));
            named.extend(
                self.roles_of
                    .get(*work)
                    .into_iter()
                    .flatten()
                    .copied()
                    .chain(delegated_role),
            );
            if opened.is_some_and(|o| o.launch == Launch::Delegation) {
                progress.delegations += 1;
                if self
                    .closes
                    .get(*work)
                    .is_some_and(|hows| hows.contains(&CloseHow::TaskDone))
                {
                    progress.delegations_done += 1;
                }
            }
        }
        named.sort_by_key(|(at, _)| *at);
        let mut roles: Vec<&String> = Vec::new();
        for (_, role) in named {
            if roles.len() == MAX_PROGRESS_ROLES {
                break;
            }
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
        progress.attempts = attempts.len() as u64;
        progress.roles = roles.into_iter().cloned().collect();
        progress
    }

    /// The launch records joined to `work`'s own attempts, latest first.
    fn records_of(&self, work: &WorkId) -> Vec<&'a LaunchRecord> {
        let mut records: Vec<&LaunchRecord> = self
            .attempts(work)
            .filter_map(|attempt| self.records.get(attempt).copied())
            .filter(|record| &record.work == work)
            .collect();
        records.sort_by_key(|record| std::cmp::Reverse(record.ts_ms));
        records
    }

    /// A short name for `work`, for the launcher field.
    fn label(&self, work: &WorkId) -> String {
        let opened = self.opened.get(work);
        let refs = opened.map(|o| o.refs);
        let by_launch = match opened.map(|o| o.launch) {
            Some(Launch::ScheduleFire) => refs.and_then(|r| r.schedule.clone()),
            Some(Launch::Orchestration) => refs.and_then(|r| r.orchestration.clone()),
            _ => None,
        };
        by_launch
            .or_else(|| {
                self.records_of(work)
                    .into_iter()
                    .find_map(|record| record.name.clone())
            })
            .or_else(|| refs.and_then(|r| r.pane.clone()))
            .unwrap_or_else(|| work.to_string())
    }

    fn item(
        &self,
        root: &'a WorkId,
        works: &[&'a WorkId],
        launcher: Option<&&'a WorkId>,
        reports: Vec<ItemReport>,
        home: &Path,
        incomplete: bool,
    ) -> InterruptedItem {
        let opened = self.opened.get(root);
        let kind = match opened.map(|o| o.launch) {
            Some(Launch::Dispatch) => ItemKind::Unit,
            Some(Launch::IssueDispatch) => ItemKind::IssueDispatch,
            Some(Launch::Orchestration | Launch::Delegation) => ItemKind::Orchestration,
            Some(Launch::Pane | Launch::ScheduleFire) | None => ItemKind::Agent,
        };
        // The root first, then the rest in the order first seen.
        let ordered: Vec<&WorkId> = std::iter::once(root)
            .chain(works.iter().copied().filter(|w| *w != root))
            .collect();
        let refs_of = |w: &WorkId| self.opened.get(w).map(|o| o.refs);
        let first_ref = |pick: &dyn Fn(&Refs) -> Option<String>| {
            ordered.iter().find_map(|w| refs_of(w).and_then(pick))
        };
        let issue = ordered
            .iter()
            .find_map(|w| refs_of(w).and_then(|r| r.issue));
        let repo = first_ref(&|r| r.repo.clone());
        let branch = first_ref(&|r| r.branch.clone());
        let orchestration = first_ref(&|r| r.orchestration.clone());
        let root_record_name = self
            .records_of(root)
            .into_iter()
            .find_map(|record| record.name.clone());
        let root_refs = refs_of(root);
        let name = match kind {
            ItemKind::Unit => root_record_name
                .or(orchestration)
                .or_else(|| root_refs.and_then(|r| r.unit_id.clone())),
            ItemKind::IssueDispatch => root_record_name.or(orchestration),
            ItemKind::Orchestration => orchestration.or_else(|| {
                ordered.iter().find_map(|w| {
                    self.records_of(w)
                        .into_iter()
                        .find_map(|record| record.orchestration.clone())
                })
            }),
            ItemKind::Agent => root_record_name
                .or_else(|| root_refs.and_then(|r| r.pane.clone()))
                .or_else(|| root_refs.and_then(|r| r.schedule.clone())),
        };
        let worktree = ordered.iter().find_map(|w| {
            let workspace = self.opened.get(w)?.workspace?;
            let records: Vec<&LaunchRecord> =
                ordered.iter().flat_map(|w| self.records_of(w)).collect();
            workspace_path(
                workspace,
                records.iter().filter_map(|record| record.cwd.as_deref()),
                home,
            )
        });
        let launcher = launcher.map(|work| self.label(work));
        let last = works
            .iter()
            .filter_map(|w| self.last_line.get(*w))
            .max()
            .and_then(|at| self.lines.lines.get(*at));
        let last_entry = LastEntry {
            kind: last.map_or("", |line| line.event.kind()).to_string(),
            ts_ms: last.map_or(0, |line| line.ts_ms),
        };
        let progress = self.progress(&ordered);
        let (agents, agents_omitted) = self.agents(kind, &ordered, incomplete);
        InterruptedItem {
            id: root.clone(),
            kind,
            name,
            issue,
            repo,
            branch,
            worktree,
            launcher,
            last_entry,
            reports,
            reports_omitted: 0,
            cut: false,
            progress,
            git: None,
            agents,
            agents_omitted,
        }
    }

    /// PRD #1790 stage 3: the item's agents, one per distinct attempt in the
    /// order its works (`ordered`, root first) name them, at most
    /// [`MAX_ITEM_AGENTS`], and how many were left out. Each verdict comes
    /// from the journal and the launch records alone.
    fn agents(
        &self,
        kind: ItemKind,
        ordered: &[&'a WorkId],
        incomplete: bool,
    ) -> (Vec<ItemAgent>, usize) {
        let mut attempts: Vec<&'a AttemptId> = Vec::new();
        for work in ordered {
            for attempt in self.attempts(work) {
                if !attempts.contains(&attempt) {
                    attempts.push(attempt);
                }
            }
        }
        // Stage 3 restores a single agent: a plain agent, or a unit or issue
        // dispatch one agent ran with nothing delegated and no orchestration.
        let team = ordered.iter().any(|w| {
            self.opened
                .get(*w)
                .is_some_and(|o| matches!(o.launch, Launch::Orchestration | Launch::Delegation))
        });
        let single = !team
            && match kind {
                ItemKind::Agent => true,
                ItemKind::Unit | ItemKind::IssueDispatch => attempts.len() == 1,
                ItemKind::Orchestration => false,
            };
        let omitted = attempts.len().saturating_sub(MAX_ITEM_AGENTS);
        attempts.truncate(MAX_ITEM_AGENTS);
        let mut sessions: Vec<Option<&str>> = Vec::new();
        let mut agents: Vec<ItemAgent> = attempts
            .iter()
            .map(|attempt| {
                let (agent, session) = self.agent(attempt, single, incomplete);
                sessions.push(session);
                agent
            })
            .collect();
        // Two agents resumed into one conversation would both write to it.
        for (at, session) in sessions.iter().enumerate() {
            let Some(session) = session else { continue };
            if sessions
                .iter()
                .enumerate()
                .any(|(other, s)| other != at && s.as_ref() == Some(session))
            {
                agents[at].restore = RestoreVerdict::NotRestorable {
                    reason: RestoreReason::AmbiguousRecord,
                };
            }
        }
        (agents, omitted)
    }

    /// One agent of an item, and the session it would be resumed into when
    /// its verdict is [`RestoreVerdict::Resume`].
    fn agent(
        &self,
        attempt: &'a AttemptId,
        single: bool,
        incomplete: bool,
    ) -> (ItemAgent, Option<&'a str>) {
        let record = self.records.get(attempt).copied();
        let session = crate::work_journal::latest_session_of(&self.lines.lines, attempt);
        let agent_type = record
            .and_then(|r| r.agent_type.clone())
            .or_else(|| session.map(|(_, agent)| agent.clone()))
            .or_else(|| {
                record.and_then(|r| crate::event::AgentType::from_command(r.command.as_deref()))
            });
        let none = |reason| RestoreVerdict::NotRestorable { reason };
        let start_over = |reason| RestoreVerdict::StartOver { reason };
        let interrupted = matches!(
            self.attempt_state.get(attempt),
            Some(AttemptState::Ended { interrupted: true } | AttemptState::Open { live: false })
        );
        let mut resumed = None;
        let restore = if incomplete {
            none(RestoreReason::IncompleteRead)
        } else if !single {
            none(RestoreReason::OrchestrationLater)
        } else if !interrupted {
            none(RestoreReason::NotInterrupted)
        } else if self.ambiguous.contains(attempt) {
            none(RestoreReason::AmbiguousRecord)
        } else if let Some(record) = record {
            let resume = agent_type
                .as_ref()
                .and_then(|agent| crate::agent_registry::spec(agent).resume);
            if record.cwd.is_none() {
                none(if record.cwd_moved_from.is_some() {
                    RestoreReason::FolderMoved
                } else {
                    RestoreReason::FolderUnknown
                })
            } else if record.command.is_none()
                && matches!(agent_type, None | Some(crate::event::AgentType::None))
            {
                start_over(RestoreReason::DefaultShell)
            } else if let Some(resume) = resume {
                match session {
                    None => start_over(RestoreReason::NoSessionId),
                    Some((id, _)) if !resume.valid_session_id(id) => {
                        start_over(RestoreReason::SessionIdInvalid)
                    }
                    Some((id, _)) => {
                        resumed = Some(id);
                        RestoreVerdict::Resume
                    }
                }
            } else {
                start_over(RestoreReason::AgentCannotResume)
            }
        } else {
            none(RestoreReason::NoLaunchRecord)
        };
        let agent = ItemAgent {
            attempt: attempt.clone(),
            name: record.and_then(|r| r.name.clone()),
            agent_type,
            role: record
                .and_then(|r| r.role.clone())
                .or_else(|| self.role_of.get(attempt).map(|role| (*role).clone())),
            folder: record.and_then(|r| r.cwd.clone()),
            restore,
        };
        (agent, resumed)
    }
}

#[cfg(test)]
#[path = "interrupted_work_tests.rs"]
mod tests;
