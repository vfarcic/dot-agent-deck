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
//! unlisted, or the line says it went to recovery. It is listed under the item
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
    /// Live Git and pull request facts about the item's worktree and branch,
    /// read when the list was asked for with them (`git: true`); absent
    /// otherwise, so the plain list's shape does not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitFacts>,
}

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

/// The most items a list answers with, so a reply stays far inside the
/// attach protocol's frame bound; [`InterruptedList::omitted`] counts the rest.
pub const MAX_LISTED_ITEMS: usize = 2_000;

/// Why a reclaim or dismissal was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalReason {
    /// The request came from an agent in a deck pane (decision D4).
    AgentCaller,
    /// The request named nothing, or named items and `all` together.
    BadRequest,
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
    /// Something this daemon runs is still rooted in the worktree.
    StillInUse,
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
pub const RECLAIM_TIMED_OUT: &str =
    "Removing its worktree did not finish in time; it may still be removed.";
pub const RECLAIM_NOT_RECORDED: &str = "Its worktree was removed and its branch kept, but the deck could not record it, so it may be listed again.";
pub const DISMISSED: &str = "Dismissed.";

pub const REFUSED_AGENT_CALLER: &str = "Refused: an agent cannot reclaim or dismiss interrupted work. Do it from the deck or from a terminal.";
pub const REFUSED_BAD_REQUEST: &str = "Refused: name the items to dismiss, or dismiss all of them.";
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
pub const REFUSED_STILL_IN_USE: &str = "Refused: an agent is still running in its worktree.";
pub const REFUSED_NOT_RECORDED: &str = "Refused: the deck could not record the dismissal.";
pub const REFUSED_UNKNOWN: &str = "Refused.";

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
        ReclaimVerdict::Refused | ReclaimVerdict::Unknown => REFUSED_UNKNOWN,
    }
}

/// The sentence for a refusal.
pub fn refusal_sentence(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::AgentCaller => REFUSED_AGENT_CALLER,
        RefusalReason::BadRequest => REFUSED_BAD_REQUEST,
        RefusalReason::Unreadable => REFUSED_UNREADABLE,
        RefusalReason::IncompleteRead => REFUSED_INCOMPLETE_READ,
        RefusalReason::NotListed => REFUSED_NOT_LISTED,
        RefusalReason::Ambiguous => REFUSED_AMBIGUOUS,
        RefusalReason::PlainAgent => REFUSED_PLAIN_AGENT,
        RefusalReason::NoWorktree => REFUSED_NO_WORKTREE,
        RefusalReason::NotLinkedWorktree => REFUSED_NOT_LINKED_WORKTREE,
        RefusalReason::BranchMismatch => REFUSED_BRANCH_MISMATCH,
        RefusalReason::NotOwned => REFUSED_NOT_OWNED,
        RefusalReason::StillInUse => REFUSED_STILL_IN_USE,
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
pub fn resolve<'a>(list: &'a InterruptedList, query: &str) -> Resolved<'a> {
    let query = query.trim();
    if query.is_empty() {
        return Resolved::NotListed;
    }
    if let Some(item) = list.items.iter().find(|item| item.id.as_str() == query) {
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
                        && (kept.adopted_unread() || reported.destination == Destination::Recovery)
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
        items.push(index.item(root, works, roots.launcher.get(root), reports, home));
    }
    items.sort_by(|a, b| {
        b.last_entry
            .ts_ms
            .cmp(&a.last_entry.ts_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    InterruptedList {
        items,
        incomplete: read.stats.truncated || launches.truncated,
        omitted: 0,
    }
}

/// The works a dismissal or reclaim of the item rooted at `root` closes: the
/// root itself, closed or not, so the item is hidden, and every work that
/// rolls up to it and has no `closed` line yet, so none of them is listed on
/// its own later. The root comes first.
pub fn works_under(read: &JournalRead, root: &WorkId) -> Vec<WorkId> {
    let no_records = LaunchRead::default();
    let index = Index::build(read, &no_records, |_| true);
    let roots = index.roots();
    let mut out = vec![root.clone()];
    if let Some(works) = roots.under.get(root) {
        out.extend(
            works
                .iter()
                .filter(|w| **w != root && !index.closes.contains_key(*w))
                .map(|w| (*w).clone()),
        );
    }
    out
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
    attempt_state: HashMap<&'a AttemptId, AttemptState>,
    closes: HashMap<&'a WorkId, Vec<CloseHow>>,
    reported: Vec<Reported<'a>>,
    /// The index in the read of each work's last line.
    last_line: HashMap<&'a WorkId, usize>,
    lines: &'a JournalRead,
    /// Launch records joined to the journal, by attempt.
    records: HashMap<&'a AttemptId, &'a LaunchRecord>,
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
            attempt_state: HashMap::new(),
            closes: HashMap::new(),
            reported: Vec::new(),
            last_line: HashMap::new(),
            lines: read,
            records: HashMap::new(),
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
                        launch: *launch,
                        parent: parent.as_ref(),
                        refs,
                        workspace: workspace.as_ref(),
                    });
                }
                Event::AttemptStarted { attempt, .. } => {
                    let list = index.attempts_of.entry(work).or_default();
                    if !list.contains(&attempt) {
                        list.push(attempt);
                    }
                    works_of.entry(attempt).or_default().insert(work);
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
            let [record] = records.as_slice() else {
                continue;
            };
            let Some((journal_attempt, works)) = works_of.get_key_value(attempt) else {
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
            git: None,
        }
    }
}

#[cfg(test)]
#[path = "interrupted_work_tests.rs"]
mod tests;
