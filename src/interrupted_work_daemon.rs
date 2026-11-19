//! PRD #1790 stage 2: the daemon side of the interrupted work verbs —
//! [`crate::daemon_protocol::AttachRequest::ListInterruptedWork`],
//! `ShowInterrupted`, `ReclaimInterrupted` and `DismissInterrupted`.
//!
//! Each one reads the journal afresh (a complete read, the launch records
//! and the kept report files) and derives the list with
//! [`crate::interrupted_work::derive`]; nothing is cached, so two clients
//! never disagree about an item. The reads and the journal writes are
//! blocking and run on `spawn_blocking`; Git runs as bounded subprocesses.
//!
//! **Who may act.** Listing and showing are open to every caller. Reclaim and
//! dismiss are refused when the request carries a pane claim
//! ([`CallerClaim`]): that is how an agent in a deck pane identifies itself
//! (the CLI sends one whenever its shell carries a pane's identity, as
//! `close` does), and stage 2 leaves these actions to the person (decision
//! D4). The claim is refused whether or not it would attest, since no agent
//! is allowed either way.
//!
//! **Reclaim** removes the item's worktree and keeps its branch. It reuses
//! [`crate::issue_dispatch_run::remove_worktree_outcome`] with
//! [`RemovalPolicy::KeepIfDirty`], so it never passes `--force` to Git and a
//! tree with uncommitted changes, or with other worktrees inside it, is kept
//! (decisions D5, D6). Before that it requires a fresh, untruncated journal
//! read in which the item is still interrupted, an item that is not a plain
//! agent's (D1), a linked worktree at the recorded path whose checked-out
//! branch is the item's, the deck's ownership marker, and nothing this daemon
//! runs rooted inside it (held for the whole removal). Only a removal writes
//! `closed how=reclaimed` on the item's works.
//!
//! **Dismiss** writes `closed how=dismissed` on every work of each item named
//! (or of every listed item), so it is no longer listed. Kept reports are left
//! to expire on schedule (D7).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::agent_pty::AgentPtyRegistry;
use crate::daemon_protocol::CallerClaim;
use crate::interrupted_work::{
    DismissOutcome, DismissRefused, DismissedItem, GitFacts, InterruptedItem, InterruptedList,
    InterruptedShow, ItemKind, MAX_LISTED_ITEMS, MAX_SHOWN_REPORT_BYTES, MAX_SHOWN_REPORTS, PrFact,
    ReclaimOutcome, ReclaimVerdict, Refusal, RefusalReason, Resolved, ShownReport, derive,
    reclaim_sentence, resolve, still_interrupted, works_under,
};
use crate::issue_dispatch_run::{RemovalPolicy, TreeKeepCheck, WorktreeRemoval};
use crate::launch_records::LaunchRead;
use crate::retained_store::{KeptReport, KeptReports, RetainedStore};
use crate::work_journal::{CloseHow, Event, JournalRead, WorkJournal};

/// The bound on each Git probe of a list's Git facts.
pub const GIT_FACT_TIMEOUT: Duration = Duration::from_secs(5);

/// The bound on one item's pull request lookup (decision D8).
pub const PR_STATE_TIMEOUT: Duration = Duration::from_secs(8);

/// The bound on a reclaim's checks and removal, as `close`'s worktree
/// cleanup has.
pub const RECLAIM_TIMEOUT: Duration = Duration::from_secs(30);

/// How many items' Git facts are read at once.
const GIT_FACT_CONCURRENCY: usize = 8;

/// One fresh read of everything the list is derived from.
struct Snapshot {
    list: InterruptedList,
    read: JournalRead,
    kept: Vec<KeptReport>,
}

/// The journal and the report store a request reads.
fn sources(
    registry: &AgentPtyRegistry,
) -> Result<(WorkJournal, Option<Arc<RetainedStore>>), String> {
    let journal = registry
        .work()
        .journal()
        .cloned()
        .ok_or_else(|| "the work journal is not open".to_string())?;
    Ok((journal, registry.inbox().retained_store()))
}

/// Read the journal, the launch records and the kept reports, and derive
/// the list. Blocking.
fn snapshot(journal: &WorkJournal, retained: Option<&RetainedStore>) -> std::io::Result<Snapshot> {
    let read = journal.read()?;
    let incomplete_launches = LaunchRead {
        truncated: true,
        ..LaunchRead::default()
    };
    let launches = match journal.state_dir() {
        Some(dir) => crate::launch_records::read_all_with_stats(dir).unwrap_or(incomplete_launches),
        None => incomplete_launches,
    };
    let kept = retained.map_or(
        KeptReports {
            reports: Vec::new(),
            incomplete: true,
        },
        RetainedStore::kept_reports,
    );
    let mut list = derive(
        &read,
        &launches,
        |run| journal.run_is_live(run),
        &kept.reports,
    );
    list.incomplete |= kept.incomplete;
    Ok(Snapshot {
        list,
        read,
        kept: kept.reports,
    })
}

/// Run `f` on the blocking pool with the request's journal and store.
async fn blocking<T: Send + 'static>(
    registry: &AgentPtyRegistry,
    f: impl FnOnce(WorkJournal, Option<Arc<RetainedStore>>) -> T + Send + 'static,
) -> Result<T, String> {
    let (journal, retained) = sources(registry)?;
    tokio::task::spawn_blocking(move || f(journal, retained))
        .await
        .map_err(|e| format!("the interrupted work read failed: {e}"))
}

/// The list, with each item's Git facts when `git` is set.
pub async fn list(registry: &AgentPtyRegistry, git: bool) -> Result<InterruptedList, String> {
    let mut list = blocking(registry, |journal, retained| {
        snapshot(&journal, retained.as_deref()).map(|snap| snap.list)
    })
    .await?
    .map_err(|e| format!("the deck could not read its work journal: {e}"))?;
    if list.items.len() > MAX_LISTED_ITEMS {
        list.omitted = list.items.len() - MAX_LISTED_ITEMS;
        list.items.truncate(MAX_LISTED_ITEMS);
    }
    if git {
        add_git_facts(&mut list.items).await;
    }
    Ok(list)
}

/// One item and its kept reports' bodies, framed as untrusted text.
pub async fn show(registry: &AgentPtyRegistry, id: String) -> Result<InterruptedShow, String> {
    blocking(registry, move |journal, retained| {
        let snap = snapshot(&journal, retained.as_deref())
            .map_err(|e| format!("the deck could not read its work journal: {e}"))?;
        let item = match resolve(&snap.list, &id) {
            Resolved::One(item) => item.clone(),
            Resolved::NotListed => {
                return Err(crate::interrupted_work::REFUSED_NOT_LISTED.to_string());
            }
            Resolved::Ambiguous => {
                return Err(crate::interrupted_work::REFUSED_AMBIGUOUS.to_string());
            }
        };
        let reports = item
            .reports
            .iter()
            .take(MAX_SHOWN_REPORTS)
            .map(|report| shown_report(&report.report_id, &snap.kept, retained.as_deref()))
            .collect();
        Ok(InterruptedShow {
            item,
            reports,
            incomplete: snap.list.incomplete,
        })
    })
    .await?
}

/// One report's body, from the file the list joined it to: one not yet
/// acknowledged.
fn shown_report(
    report_id: &str,
    kept: &[KeptReport],
    retained: Option<&RetainedStore>,
) -> ShownReport {
    let mut shown = ShownReport {
        report_id: report_id.to_string(),
        body: None,
        cut: false,
        error: None,
    };
    let Some(file) = kept
        .iter()
        .find(|k| k.report_id == report_id && !k.acknowledged())
    else {
        shown.error = Some("its file is no longer kept".to_string());
        return shown;
    };
    let Some(store) = retained else {
        shown.error = Some("the report store cannot be used".to_string());
        return shown;
    };
    match store.read_kept_report(file, MAX_SHOWN_REPORT_BYTES) {
        Ok(body) => {
            shown.body = Some(body.framed);
            shown.cut = body.cut;
        }
        Err(e) => shown.error = Some(format!("it could not be read: {e}")),
    }
    shown
}

/// What a reclaim checked before it may remove anything.
struct Prepared {
    item: InterruptedItem,
    read: JournalRead,
    journal: WorkJournal,
    /// The recorded path, as the item names it.
    raw: PathBuf,
    /// Its canonical form, a linked worktree's top level.
    worktree: PathBuf,
    /// The main worktree of its repository.
    clone: PathBuf,
    branch: String,
}

/// The checks of a reclaim that read the journal and the filesystem.
/// Blocking.
fn prepare_reclaim(
    journal: WorkJournal,
    retained: Option<&RetainedStore>,
    id: &str,
    dry_run: bool,
) -> Result<Prepared, Box<ReclaimOutcome>> {
    let refuse =
        |id: Option<String>, reason| Box::new(ReclaimOutcome::refused(id, dry_run, reason));
    let snap = snapshot(&journal, retained).map_err(|_| refuse(None, RefusalReason::Unreadable))?;
    if snap.read.stats.truncated {
        return Err(refuse(None, RefusalReason::IncompleteRead));
    }
    let item = match resolve(&snap.list, id) {
        Resolved::One(item) => item.clone(),
        Resolved::NotListed => return Err(refuse(None, RefusalReason::NotListed)),
        Resolved::Ambiguous => return Err(refuse(None, RefusalReason::Ambiguous)),
    };
    let refuse_item = |reason| {
        let mut outcome = refuse(Some(item.id.to_string()), reason);
        outcome.worktree = item.worktree.clone();
        outcome.branch = item.branch.clone();
        outcome
    };
    if item.kind == ItemKind::Agent {
        return Err(refuse_item(RefusalReason::PlainAgent));
    }
    if !still_interrupted(&snap.read, &item.id, |run| journal.run_is_live(run)) {
        return Err(refuse_item(RefusalReason::NotListed));
    }
    let Some(raw) = item.worktree.as_deref().map(PathBuf::from) else {
        return Err(refuse_item(RefusalReason::NoWorktree));
    };
    let worktree = match std::fs::canonicalize(&raw) {
        Ok(path) if path.is_dir() => path,
        _ => return Err(refuse_item(RefusalReason::NoWorktree)),
    };
    let clone = match crate::worktree_owner::verified_geometry_of(&worktree) {
        Some(geometry) if geometry.toplevel == worktree && geometry.main != worktree => {
            geometry.main
        }
        _ => return Err(refuse_item(RefusalReason::NotLinkedWorktree)),
    };
    let Some(branch) = item.branch.clone() else {
        return Err(refuse_item(RefusalReason::BranchMismatch));
    };
    if !crate::worktree_owner::is_marked(&worktree) {
        return Err(refuse_item(RefusalReason::NotOwned));
    }
    Ok(Prepared {
        item,
        read: snap.read,
        journal,
        raw,
        worktree,
        clone,
        branch,
    })
}

/// What the removal step answered.
enum Removal {
    InUse,
    Done(ReclaimVerdict, Vec<String>),
}

/// Reclaim the item `id` names: remove its worktree, keep its branch. With
/// `dry_run`, report the verdict it would reach and remove nothing.
pub async fn reclaim(
    registry: &Arc<AgentPtyRegistry>,
    id: String,
    dry_run: bool,
    caller: Option<&CallerClaim>,
) -> ReclaimOutcome {
    if caller.is_some() {
        return ReclaimOutcome::refused(None, dry_run, RefusalReason::AgentCaller);
    }
    let prepared = match blocking(registry, move |journal, retained| {
        prepare_reclaim(journal, retained.as_deref(), &id, dry_run)
    })
    .await
    {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(refused)) => return *refused,
        Err(_) => return ReclaimOutcome::refused(None, dry_run, RefusalReason::Unreadable),
    };
    let Prepared {
        item,
        read,
        journal,
        raw,
        worktree,
        clone,
        branch,
    } = prepared;
    let shown_path = worktree.to_string_lossy().into_owned();
    let refuse = |reason| {
        let mut outcome = ReclaimOutcome::refused(Some(item.id.to_string()), dry_run, reason);
        outcome.worktree = Some(shown_path.clone());
        outcome.branch = Some(branch.clone());
        outcome
    };
    if head_branch(&worktree).await.as_deref() != Some(branch.as_str()) {
        return refuse(RefusalReason::BranchMismatch);
    }
    let unpushed_commits = unpushed_commits(&worktree).await;

    // The hold and the removal run in their own task, as `close`'s cleanup
    // does: a removal that outlives the bound keeps its hold until it ends.
    let task = {
        let registry = Arc::clone(registry);
        let worktree = worktree.clone();
        tokio::spawn(async move {
            let Some(_hold) = registry.hold_dir_for_removal(&worktree) else {
                return Removal::InUse;
            };
            let _raw_hold = if raw == worktree {
                None
            } else {
                match registry.hold_dir_for_removal(&raw) {
                    Some(hold) => Some(hold),
                    None => return Removal::InUse,
                }
            };
            if dry_run {
                return match crate::issue_dispatch_run::worktree_keep_check(&worktree, &clone).await
                {
                    Ok(TreeKeepCheck::Clean) => Removal::Done(ReclaimVerdict::WouldRemove, vec![]),
                    Ok(TreeKeepCheck::Dirty) => Removal::Done(ReclaimVerdict::KeptDirty, vec![]),
                    Ok(TreeKeepCheck::HoldsWorktrees(nested)) => {
                        Removal::Done(ReclaimVerdict::KeptHoldsWorktrees, nested)
                    }
                    Err(_) => Removal::Done(ReclaimVerdict::KeptCouldNotCheck, vec![]),
                };
            }
            match crate::issue_dispatch_run::remove_worktree_outcome(
                &worktree,
                &clone,
                RemovalPolicy::KeepIfDirty,
            )
            .await
            {
                WorktreeRemoval::Removed => Removal::Done(ReclaimVerdict::Removed, vec![]),
                WorktreeRemoval::KeptDirty => Removal::Done(ReclaimVerdict::KeptDirty, vec![]),
                WorktreeRemoval::KeptCouldNotCheck => {
                    Removal::Done(ReclaimVerdict::KeptCouldNotCheck, vec![])
                }
                WorktreeRemoval::RemoveFailed => {
                    Removal::Done(ReclaimVerdict::RemoveFailed, vec![])
                }
                WorktreeRemoval::KeptHoldsWorktrees(nested) => {
                    Removal::Done(ReclaimVerdict::KeptHoldsWorktrees, nested)
                }
            }
        })
    };
    let (verdict, nested_worktrees) = match tokio::time::timeout(RECLAIM_TIMEOUT, task).await {
        Ok(Ok(Removal::Done(verdict, nested))) => (verdict, nested),
        Ok(Ok(Removal::InUse)) => return refuse(RefusalReason::StillInUse),
        Ok(Err(join_error)) => {
            tracing::warn!(error = %join_error, "interrupted work: the reclaim task failed");
            (ReclaimVerdict::RemoveFailed, Vec::new())
        }
        Err(_) => (ReclaimVerdict::TimedOut, Vec::new()),
    };
    let mut outcome = ReclaimOutcome {
        id: Some(item.id.to_string()),
        dry_run,
        verdict,
        message: reclaim_sentence(verdict).to_string(),
        refusal: None,
        worktree: Some(shown_path.clone()),
        branch: Some(branch.clone()),
        unpushed_commits,
        nested_worktrees,
        recorded: false,
    };
    if verdict == ReclaimVerdict::Removed {
        let root = item.id.clone();
        outcome.recorded = tokio::task::spawn_blocking(move || {
            close_all(&journal, &works_under(&read, &root), CloseHow::Reclaimed)
        })
        .await
        .unwrap_or(false);
        if !outcome.recorded {
            outcome.message = crate::interrupted_work::RECLAIM_NOT_RECORDED.to_string();
        }
    }
    outcome
}

/// Dismiss the items `ids` name, or every listed item with `all`.
pub async fn dismiss(
    registry: &AgentPtyRegistry,
    ids: Vec<String>,
    all: bool,
    caller: Option<&CallerClaim>,
) -> DismissOutcome {
    let refused = |reason| DismissOutcome {
        refusal: Some(Refusal::new(reason)),
        ..DismissOutcome::default()
    };
    if caller.is_some() {
        return refused(RefusalReason::AgentCaller);
    }
    if all == !ids.is_empty() {
        return refused(RefusalReason::BadRequest);
    }
    let answer = blocking(registry, move |journal, retained| {
        let Ok(snap) = snapshot(&journal, retained.as_deref()) else {
            return refused(RefusalReason::Unreadable);
        };
        let mut outcome = DismissOutcome {
            incomplete: snap.list.incomplete,
            ..DismissOutcome::default()
        };
        let mut targets: Vec<&InterruptedItem> = Vec::new();
        if all {
            targets.extend(snap.list.items.iter());
        } else {
            for id in &ids {
                match resolve(&snap.list, id) {
                    Resolved::One(item) => {
                        if !targets.iter().any(|t| t.id == item.id) {
                            targets.push(item);
                        }
                    }
                    Resolved::NotListed => outcome.refused.push(DismissRefused {
                        id: id.clone(),
                        refusal: Refusal::new(RefusalReason::NotListed),
                    }),
                    Resolved::Ambiguous => outcome.refused.push(DismissRefused {
                        id: id.clone(),
                        refusal: Refusal::new(RefusalReason::Ambiguous),
                    }),
                }
            }
        }
        for item in targets {
            if close_all(
                &journal,
                &works_under(&snap.read, &item.id),
                CloseHow::Dismissed,
            ) {
                outcome.dismissed.push(DismissedItem {
                    id: item.id.to_string(),
                    name: item.name.clone(),
                });
            } else {
                outcome.refused.push(DismissRefused {
                    id: item.id.to_string(),
                    refusal: Refusal::new(RefusalReason::NotRecorded),
                });
            }
        }
        outcome
    })
    .await;
    answer.unwrap_or_else(|_| refused(RefusalReason::Unreadable))
}

/// Write `closed how=<how>` on every work, waiting for each write; `true`
/// when all were written. Blocking.
fn close_all(journal: &WorkJournal, works: &[crate::work_journal::WorkId], how: CloseHow) -> bool {
    let mut all = true;
    for work in works {
        if let Err(error) = journal.try_record(Event::Closed {
            work: work.clone(),
            how,
        }) {
            tracing::warn!(?error, %work, "interrupted work: could not record the close");
            all = false;
        }
    }
    all
}

/// The branch checked out in `worktree`, or `None` when `HEAD` is detached
/// or Git did not answer.
async fn head_branch(worktree: &Path) -> Option<String> {
    let dir = worktree.to_string_lossy();
    crate::issue_dispatch_run::run_git_capture_within(
        &["-C", &dir, "symbolic-ref", "--quiet", "--short", "HEAD"],
        GIT_FACT_TIMEOUT,
    )
    .await
    .ok()
    .map(|out| out.trim().to_string())
    .filter(|branch| !branch.is_empty())
}

/// How many commits `HEAD` has that no remote-tracking branch holds.
async fn unpushed_commits(worktree: &Path) -> Option<u64> {
    let dir = worktree.to_string_lossy();
    crate::issue_dispatch_run::run_git_capture_within(
        &[
            "-C",
            &dir,
            "rev-list",
            "--count",
            "HEAD",
            "--not",
            "--remotes",
        ],
        GIT_FACT_TIMEOUT,
    )
    .await
    .ok()?
    .trim()
    .parse()
    .ok()
}

/// Whether `branch` is a local branch of `worktree`'s repository.
async fn branch_exists(worktree: &Path, branch: &str) -> Option<bool> {
    let dir = worktree.to_string_lossy();
    let full = format!("refs/heads/{branch}");
    let out = crate::issue_dispatch_run::run_git_capture_within(
        &["-C", &dir, "for-each-ref", "--format=%(refname)", &full],
        GIT_FACT_TIMEOUT,
    )
    .await
    .ok()?;
    Some(out.lines().any(|line| line == full))
}

/// The state of `branch`'s pull request, within [`PR_STATE_TIMEOUT`].
async fn pr_state(worktree: &Path, branch: &str) -> PrFact {
    let (dir, branch) = (worktree.to_path_buf(), branch.to_string());
    let lookup = tokio::task::spawn_blocking(move || {
        crate::worktree_reclaim::resolve_pr_state(&dir, &branch)
    });
    use crate::worktree_reclaim::PrState;
    match tokio::time::timeout(PR_STATE_TIMEOUT, lookup).await {
        Ok(Ok(PrState::Open)) => PrFact::Open,
        Ok(Ok(PrState::Merged)) => PrFact::Merged,
        Ok(Ok(PrState::ClosedUnmerged)) => PrFact::Closed,
        Ok(Ok(PrState::NoPr)) => PrFact::None,
        _ => PrFact::Unknown,
    }
}

/// One item's Git facts. A plain agent's directory is not a worktree the
/// deck made, so only whether it exists is read for it.
async fn git_facts(kind: ItemKind, worktree: Option<String>, branch: Option<String>) -> GitFacts {
    let Some(worktree) = worktree else {
        return GitFacts::default();
    };
    let path = PathBuf::from(worktree);
    let worktree_exists = match tokio::fs::metadata(&path).await {
        Ok(meta) => Some(meta.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    let mut facts = GitFacts {
        worktree_exists,
        ..GitFacts::default()
    };
    if worktree_exists != Some(true) || kind == ItemKind::Agent {
        return facts;
    }
    let dirty = async {
        tokio::time::timeout(
            GIT_FACT_TIMEOUT,
            crate::issue_dispatch_run::worktree_is_dirty(&path),
        )
        .await
        .ok()
        .and_then(Result::ok)
    };
    let branch = branch.as_deref();
    let exists = async {
        match branch {
            Some(branch) => branch_exists(&path, branch).await,
            None => None,
        }
    };
    let pr = async {
        match branch {
            Some(branch) => pr_state(&path, branch).await,
            None => PrFact::Unknown,
        }
    };
    let (dirty, unpushed, exists, pr) = tokio::join!(dirty, unpushed_commits(&path), exists, pr);
    facts.dirty = dirty;
    facts.unpushed_commits = unpushed;
    facts.branch_exists = exists;
    facts.pr = pr;
    facts
}

/// Fill each item's Git facts, [`GIT_FACT_CONCURRENCY`] items at a time.
async fn add_git_facts(items: &mut [InterruptedItem]) {
    let permits = Arc::new(tokio::sync::Semaphore::new(GIT_FACT_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for (at, item) in items.iter().enumerate() {
        let (kind, worktree, branch) = (item.kind, item.worktree.clone(), item.branch.clone());
        let permits = Arc::clone(&permits);
        set.spawn(async move {
            let _permit = permits.acquire_owned().await;
            (at, git_facts(kind, worktree, branch).await)
        });
    }
    for item in items.iter_mut() {
        item.git = Some(GitFacts::default());
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((at, facts)) = joined
            && let Some(item) = items.get_mut(at)
        {
            item.git = Some(facts);
        }
    }
}

#[cfg(all(test, unix))]
#[path = "interrupted_work_daemon_tests.rs"]
mod tests;
