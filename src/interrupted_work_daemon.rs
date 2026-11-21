//! PRD #1790 stage 2: the daemon side of the interrupted work verbs —
//! [`crate::daemon_protocol::AttachRequest::ListInterruptedWork`],
//! `ShowInterrupted`, `ReclaimInterrupted` and `DismissInterrupted`.
//!
//! Each request reads the journal afresh (a complete read, the launch records
//! and the kept report files) and derives the list with
//! [`crate::interrupted_work::derive`]; nothing is cached between requests.
//! That does not make two clients agree: requests read at different times,
//! and the journal, the launch records and the report files are read one
//! after another, so recovery or adoption can change one source while
//! another is being read. The reads and the journal writes are blocking and
//! run on `spawn_blocking`. The `git` and `gh` children of a list's facts and
//! of a reclaim's geometry, marker and branch checks run through
//! [`crate::interrupted_work_probe`] (a deadline, an output cap, the process
//! group killed and the child reaped at the deadline, and a daemon-wide limit
//! on how many run at once); the reclaim's dirty and nested-worktree check
//! and its removal run through [`crate::issue_dispatch_run`]'s runner, which
//! kills the process group at its own bound.
//!
//! **Replies are bounded.** A list keeps at most
//! [`crate::interrupted_work::MAX_LISTED_ITEMS`] items and at most
//! [`MAX_REPLY_BYTES`] of JSON, each item at most
//! [`crate::interrupted_work::MAX_LISTED_REPORTS`] report entries and each
//! string at most [`crate::interrupted_work::MAX_FIELD_BYTES`]; a show bounds
//! its report entries and bodies the same way ([`fit_list`],
//! [`fit_show_page`]), and either says what it left out. A show can ask for
//! any one of the item's reports by id, and for its reports a page at a time
//! (`reports_from`), each answer saying where the next page starts, so every
//! kept report can be found past the list's cap. What a reclaim or a dismissal acts on is resolved from the
//! unfitted list, never from what a reply carried.
//!
//! **Who may act.** Listing and showing are open to every caller. Reclaim and
//! dismiss are refused when the request carries a pane claim
//! ([`CallerClaim`]): that is how an agent in a deck pane identifies itself
//! (the CLI sends one whenever its shell carries a pane's identity, as
//! `close` does), and stage 2 leaves these actions to the person (decision
//! D4). The claim is refused whether or not it would attest, since no agent
//! is allowed either way. **This is a guard for a cooperative agent, not an
//! authentication boundary**, as `close`'s is (`src/close_agents.rs`): the
//! claim is optional, so a same-uid process that drops its pane identity, or
//! sends the request without the field, is treated as the person
//! (`docs/develop/hook-provenance.md` has the trust boundary).
//!
//! **Reclaim** removes the item's worktree and keeps its branch. It reuses
//! [`crate::issue_dispatch_run::remove_worktree_outcome_guarded`] with
//! [`RemovalPolicy::KeepIfDirty`], so it never passes `--force` to Git and a
//! tree with uncommitted changes, or with other worktrees inside it, is kept
//! (decisions D5, D6). Before that it requires a fresh, untruncated journal
//! read in which the item is still interrupted, an item that is not a plain
//! agent's (D1), a linked worktree at the recorded path whose checked-out
//! branch is the item's, and the deck's ownership marker naming the item's
//! own launch ([`crate::worktree_owner::marked_launch_in`], PR #2431 round 2).
//! An issue dispatched again at the same path and on the same branch passes
//! the branch check and carries a marker too, so only the launch the marker
//! names tells that run's tree from the older item's: a marker naming
//! another launch refuses ([`RefusalReason::NewerRun`]), and so does one
//! naming none ([`RefusalReason::OwnerUnconfirmed`]: a marker an older build
//! wrote, or one that cannot be read). The decision never rests on the
//! journal holding no record of a later run, since the journal is best
//! effort and such a record can be missing. An item's root is the launch's
//! own work for a dispatched unit and an issue dispatch, which are the
//! launches that create worktrees, so it is compared with the root alone.
//! It then holds the
//! worktree against new spawns ([`AgentPtyRegistry::hold_dir_for_removal`],
//! by its recorded and its canonical path) and, with the hold taken,
//! resolves every directory anything this daemon runs or is starting is
//! rooted in to its canonical form, outside the registry lock: one inside the
//! worktree refuses the reclaim, and so does one that cannot be resolved. A
//! spawn reserved after the hold into an alias of the worktree is refused by
//! it, since a spawn whose directory resolves records its canonical spelling
//! too. It then takes the per-repository worktree lock, waiting at most
//! [`RECLAIM_LOCK_WAIT`] for it; when it cannot be taken nothing is
//! removed: [`ReclaimVerdict::KeptBusy`] when it was not obtained before the
//! wait ran out, [`ReclaimVerdict::KeptCouldNotLock`] when it could not be
//! taken for another reason (its path not resolved, its file not opened or
//! locked). The wait tries the lock without blocking and sleeps between
//! tries, so on Unix a reclaim that gives up leaves nothing waiting for the
//! lock ([`crate::platform::lock::acquire_spawn_lock_within`]); it is not
//! fair, so steady short holders can outlast it. That is the lock the deck takes to create
//! a dispatch's worktree ([`crate::issue_dispatch_run::create_worktree`],
//! which goes on without it after waiting 60 seconds) and to remove a
//! worktree a spawn abandoned; removing a dispatched worktree when its agent
//! closes does not take it. Under that lock it runs the dirty and
//! nested-worktree check, then, immediately before `git worktree remove`,
//! re-validates that the directory is the one it checked: the same device and
//! inode at the same canonical path, the owner marker in the same git
//! metadata directory and still naming the item's launch (read again, so a
//! later launch that created the tree again since the first check is
//! caught), the item's branch checked out, no other worktree
//! inside it (asked again), and last the same device and inode once more,
//! since the probes in between take time. Any difference keeps the tree.
//!
//! **What reclaim does not guard against.** The hold refuses spawns this
//! daemon starts, and the lock excludes only the operations that take it
//! (above). Neither stops another process
//! (a terminal, a Git command, another tool) from moving, replacing or
//! switching the worktree, or adding a worktree inside it. The re-validation
//! checks each fact once, at its own point, and each has its own window
//! between that check and `git worktree remove` acting on the path, which
//! nothing here closes. The directory's identity is checked last (the second
//! device and inode check), so a process that swaps another clean tree in at
//! that path after it can still have that tree removed: `git worktree remove`
//! resolves the path itself. The owner marker, the branch and the set of
//! worktrees inside it are checked earlier, and changing one of them in place
//! changes neither the directory's device nor its inode, so the identity
//! check after them does not notice: a marker removed or rewritten or a
//! branch switched after its own check, or a worktree added inside the tree after the nested
//! list was taken, is not seen, and that nested worktree is removed with it.
//! Without `--force`, Git refuses to remove a worktree in which it reports
//! modified tracked files or untracked files. That covers only what Git
//! reports: ignored files are removed, and so are changes to a tracked file
//! Git is told not to look at (`git update-index --assume-unchanged` or
//! `--skip-worktree`).
//!
//! A removal that succeeds writes `closed how=reclaimed` on the item's works
//! from the task that owns the removal, while it still holds the worktree,
//! so a removal that finishes after the reply's bound is recorded too.
//!
//! **Dismiss** writes `closed how=dismissed` on every work of each item named
//! (or of every listed item), so it is no longer listed. Kept reports are left
//! to expire on schedule (D7). A truncated journal read refuses the whole
//! dismissal, as it refuses a reclaim, since it may have left out works under
//! an item that the dismissal would then not close. Its selector is bounded before anything is
//! read ([`MAX_DISMISS_IDS`], [`MAX_DISMISS_ID_BYTES`], [`MAX_DISMISS_BYTES`]),
//! the journal is indexed once per request ([`CloseSets`]), and its answer
//! is bounded: its lists are capped
//! ([`crate::interrupted_work::MAX_DISMISS_REPORTED`]), each dismissed item's
//! name is cut to [`crate::interrupted_work::MAX_FIELD_BYTES`], and the whole
//! answer is fitted to [`MAX_REPLY_BYTES`], counting the entries it leaves
//! out ([`DismissOutcome::fit`]). The closes are written before the answer is
//! fitted, so fitting changes what the answer names, never what was done.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::agent_pty::AgentPtyRegistry;
use crate::daemon_protocol::CallerClaim;
use crate::interrupted_work::{
    CloseSets, DismissOutcome, DismissedItem, GIT_FACTS_RESERVE, GitFacts, InterruptedItem,
    InterruptedList, InterruptedShow, ItemKind, MAX_REPLY_BYTES, MAX_SHOWN_REPORT_BYTES,
    MAX_SHOWN_REPORTS, PrFact, REPORT_NOT_OF_ITEM, ReclaimOutcome, ReclaimVerdict, RefusalReason,
    Resolved, ShownReport, derive, fit_list, fit_show_page, reclaim_sentence, resolve,
    still_interrupted,
};
use crate::interrupted_work_probe::{self as probe, Deadline};
use crate::issue_dispatch_run::{
    RemovalGuard, RemovalPolicy, TreeKeepCheck, WorktreeLockUnavailable, WorktreeRemoval,
};
use crate::launch_records::LaunchRead;
use crate::retained_store::{KeptReport, KeptReports, RetainedStore};
use crate::work_journal::{CloseHow, Event, JournalRead, WorkId, WorkJournal};
use crate::worktree_owner::MarkedLaunch;

/// The bound on one item's Git facts.
pub const GIT_FACT_TIMEOUT: Duration = Duration::from_secs(5);

/// The bound on one item's pull request lookup (decision D8), all its `git`
/// and `gh` children together.
pub const PR_STATE_TIMEOUT: Duration = Duration::from_secs(8);

/// The bound on all of one list's Git and pull request facts: an item not
/// started by then gets none, and one running then is cut short.
pub const LIST_ENRICHMENT_BUDGET: Duration = Duration::from_secs(20);

/// The bound on a reclaim's checks before its removal: the worktree's
/// geometry, its branch and its unpushed commits.
pub const RECLAIM_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The bound on the re-validation just before the removal.
pub const REVALIDATE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a reclaim waits for the repository's worktree lock.
pub const RECLAIM_LOCK_WAIT: Duration = Duration::from_secs(15);

/// The bound on a reclaim's removal, as `close`'s worktree cleanup has. The
/// removal is not stopped at it, only the reply.
pub const RECLAIM_TIMEOUT: Duration = Duration::from_secs(30);

/// How many items' Git facts one list reads at once.
const GIT_FACT_CONCURRENCY: usize = 8;

/// The most ids one dismissal may name, as `close`'s selector
/// ([`crate::close_agents::MAX_SELECTOR_ENTRIES`]).
pub const MAX_DISMISS_IDS: usize = crate::close_agents::MAX_SELECTOR_ENTRIES;

/// The longest id a dismissal may name, in bytes.
pub const MAX_DISMISS_ID_BYTES: usize = crate::close_agents::MAX_SELECTOR_ENTRY_BYTES;

/// The most bytes all of a dismissal's ids may add up to.
pub const MAX_DISMISS_BYTES: usize = crate::close_agents::MAX_SELECTOR_BYTES;

/// The `gh` a list runs for pull request state: `gh` on the daemon's `PATH`.
const GH: &str = "gh";

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

/// The list, fitted to [`MAX_REPLY_BYTES`], with each item's Git facts when
/// `git` is set.
pub async fn list(registry: &AgentPtyRegistry, git: bool) -> Result<InterruptedList, String> {
    list_with(registry, git, OsStr::new(GH)).await
}

/// [`list`], running `gh` for pull request state.
async fn list_with(
    registry: &AgentPtyRegistry,
    git: bool,
    gh: &OsStr,
) -> Result<InterruptedList, String> {
    let mut list = blocking(registry, |journal, retained| {
        snapshot(&journal, retained.as_deref()).map(|snap| snap.list)
    })
    .await?
    .map_err(|e| format!("the deck could not read its work journal: {e}"))?;
    let reserve = if git { GIT_FACTS_RESERVE } else { 0 };
    fit_list(&mut list, MAX_REPLY_BYTES, reserve);
    if git {
        add_git_facts(&mut list.items, gh, Deadline::after(LIST_ENRICHMENT_BUDGET)).await;
    }
    Ok(list)
}

/// One item and its kept reports' bodies, framed as untrusted text: the
/// first [`MAX_SHOWN_REPORTS`] of them, or, with `report`, that one report of
/// the item's.
pub async fn show(
    registry: &AgentPtyRegistry,
    id: String,
    report: Option<String>,
) -> Result<InterruptedShow, String> {
    show_page(registry, id, report, None).await
}

/// [`show`] of one page of the item's kept reports: with `reports_from`, the
/// entries from the item's `reports_from`-th report on (at most
/// [`MAX_SHOWN_REPORTS`]) and, without `report`, their bodies; the answer
/// says where the next page starts
/// ([`crate::interrupted_work::fit_show_page`]).
pub async fn show_page(
    registry: &AgentPtyRegistry,
    id: String,
    report: Option<String>,
    reports_from: Option<usize>,
) -> Result<InterruptedShow, String> {
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
        let bodies = shown_bodies_page(&item, report.as_deref(), reports_from)?;
        Ok(fit_show_page(
            item,
            reports_from,
            &bodies,
            snap.list.incomplete,
            MAX_REPLY_BYTES,
            |report_id| shown_report(report_id, &snap.kept, retained.as_deref()),
        ))
    })
    .await?
}

/// The report ids a show reads bodies for: `report`, when it is one of
/// `item`'s, else the item's first [`MAX_SHOWN_REPORTS`]. Matched against
/// every report of the item, not only the ones a reply would carry.
fn shown_bodies(item: &InterruptedItem, report: Option<&str>) -> Result<Vec<String>, String> {
    match report {
        Some(wanted) => item
            .reports
            .iter()
            .find(|r| r.report_id == wanted)
            .map(|r| vec![r.report_id.clone()])
            .ok_or_else(|| REPORT_NOT_OF_ITEM.to_string()),
        None => Ok(item
            .reports
            .iter()
            .take(MAX_SHOWN_REPORTS)
            .map(|r| r.report_id.clone())
            .collect()),
    }
}

/// The report ids a show of the page starting at the item's `page`-th
/// report reads bodies for: `report`, when one is asked for (any of the
/// item's), else the page's entries, at most [`MAX_SHOWN_REPORTS`]
/// ([`crate::interrupted_work::fit_show_page`] carries the same ones). With
/// no page, [`shown_bodies`].
fn shown_bodies_page(
    item: &InterruptedItem,
    report: Option<&str>,
    page: Option<usize>,
) -> Result<Vec<String>, String> {
    match (report, page) {
        (None, Some(from)) => Ok(item
            .reports
            .iter()
            .skip(from)
            .take(MAX_SHOWN_REPORTS)
            .map(|r| r.report_id.clone())
            .collect()),
        _ => shown_bodies(item, report),
    }
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

/// A directory's identity on disk: its device and inode. `None` off Unix,
/// where only the canonical path is compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirIdentity(Option<(u64, u64)>);

impl DirIdentity {
    fn of(dir: &Path) -> Option<Self> {
        let meta = std::fs::metadata(dir).ok()?;
        if !meta.is_dir() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(DirIdentity(Some((meta.dev(), meta.ino()))))
        }
        #[cfg(not(unix))]
        {
            Some(DirIdentity(None))
        }
    }
}

/// What a reclaim read from the journal, before it asks Git anything.
struct Prepared {
    item: InterruptedItem,
    journal: WorkJournal,
    /// The works a reclaim closes, root first.
    works: Vec<WorkId>,
    /// The recorded path, as the item names it.
    raw: PathBuf,
    /// Its canonical form.
    worktree: PathBuf,
    /// The canonical path's identity when it was resolved.
    identity: DirIdentity,
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
    let Some((worktree, identity)) = std::fs::canonicalize(&raw)
        .ok()
        .and_then(|path| DirIdentity::of(&path).map(|identity| (path, identity)))
    else {
        return Err(refuse_item(RefusalReason::NoWorktree));
    };
    let works = CloseSets::build(&snap.read).works_under(&item.id);
    Ok(Prepared {
        item,
        journal,
        works,
        raw,
        worktree,
        identity,
    })
}

/// What the removal task answered.
enum Removal {
    InUse,
    InUseUnknown,
    Done {
        verdict: ReclaimVerdict,
        nested: Vec<String>,
        recorded: bool,
    },
}

impl Removal {
    fn done(verdict: ReclaimVerdict) -> Self {
        Removal::Done {
            verdict,
            nested: Vec::new(),
            recorded: false,
        }
    }
}

/// Whether anything rooted in `worktree` (canonical) is running or starting.
enum InUse {
    No,
    Yes,
    /// A rooted directory could not be resolved, so it is not known.
    Unknown,
}

/// The canonical form of `path`. A path that is gone resolves through its
/// nearest existing ancestor, with the missing names appended; `None` when
/// that cannot be done (a `..` among the missing names, a directory that
/// cannot be read).
fn canonical_identity(path: &Path) -> Option<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => Some(canonical),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let name = path.file_name()?;
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
            canonical_identity(parent).map(|parent| parent.join(name))
        }
        Err(_) => None,
    }
}

/// Whether anything `registry` runs or is starting is rooted in `worktree`,
/// with every rooted directory resolved to its canonical form outside the
/// registry lock, so a pane started through a symlink to the worktree, or a
/// path with `..` in it, is seen.
async fn in_use_by_identity(registry: &AgentPtyRegistry, worktree: &Path) -> InUse {
    let dirs = registry.rooted_dirs_snapshot();
    let worktree = worktree.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut unknown = false;
        for dir in dirs {
            match canonical_identity(&dir) {
                Some(canonical) if canonical.starts_with(&worktree) => return InUse::Yes,
                Some(_) => {}
                None => unknown = true,
            }
        }
        if unknown { InUse::Unknown } else { InUse::No }
    })
    .await
    .unwrap_or(InUse::Unknown)
}

/// What a reclaim checked, for re-validating just before the removal.
#[derive(Clone)]
struct Checked {
    raw: PathBuf,
    worktree: PathBuf,
    identity: DirIdentity,
    clone: PathBuf,
    git_dir: PathBuf,
    branch: String,
    /// The item's root, which the marker must still name.
    launch: WorkId,
}

/// The launch the marker in `git_dir` names, read on the blocking pool;
/// [`MarkedLaunch::Unnamed`] when the read task fails.
async fn marked_launch(git_dir: &Path) -> MarkedLaunch {
    let git_dir = git_dir.to_path_buf();
    tokio::task::spawn_blocking(move || crate::worktree_owner::marked_launch_in(&git_dir))
        .await
        .unwrap_or(MarkedLaunch::Unnamed)
}

/// Whether the directory at the reclaimed path is still the one the reclaim
/// checked, the same device and inode at the same canonical path. Blocking.
fn same_directory(checked: &Checked) -> bool {
    std::fs::canonicalize(&checked.raw).ok().as_ref() == Some(&checked.worktree)
        && DirIdentity::of(&checked.worktree) == Some(checked.identity)
}

/// [`same_directory`] on the blocking pool.
async fn still_the_same_directory(checked: &Checked) -> bool {
    let checked = checked.clone();
    tokio::task::spawn_blocking(move || same_directory(&checked))
        .await
        .unwrap_or(false)
}

/// The program a reclaim's keep check runs its `git status` and
/// `git worktree list` as, and the bound on that check: `git` and
/// [`RECLAIM_CHECK_TIMEOUT`], unless a test armed its seam for `worktree`.
fn keep_check_runner(worktree: &Path) -> (Option<PathBuf>, Duration) {
    #[cfg(test)]
    if let Some((program, bound)) = seams::keep_check(worktree) {
        return (Some(program), bound);
    }
    let _ = worktree;
    (None, RECLAIM_CHECK_TIMEOUT)
}

/// One keep-check child through the bounded probe runner
/// ([`probe::run`]): its output capped, admission held until it has been
/// reaped, and killed with its process group at `deadline`.
async fn keep_check_child(
    program: Option<&Path>,
    dir: &Path,
    args: &[&str],
    deadline: Deadline,
) -> Result<Vec<u8>, String> {
    let program = match program {
        Some(path) => probe::Program::Other(path.as_os_str()),
        None => probe::Program::Git,
    };
    probe::run(program, dir, args, deadline).await
}

/// Final review, auditor 1 and 3: the worktrees inside `worktree`, bounded
/// by `deadline` — `clone`'s repository's listing, taken through the
/// bounded runner (output past its cap is a check not made), and a look at
/// the directory for any other repository's.
async fn bounded_nested(
    worktree: &Path,
    clone: &Path,
    program: Option<&Path>,
    deadline: Deadline,
) -> Result<Vec<String>, String> {
    let args = ["worktree", "list", "--porcelain", "-z"];
    let (listing, found) = tokio::join!(
        keep_check_child(program, clone, &args, deadline),
        crate::issue_dispatch_run::git_trees_inside(worktree, deadline.as_std())
    );
    let listed = match listing {
        Ok(out) => crate::issue_dispatch_run::nested_in_listing(worktree, &out).await,
        Err(e) => Err(e),
    };
    crate::issue_dispatch_run::merge_nested(listed, found)
}

/// Final review, auditor 3: a reclaim's keep check, the question
/// [`crate::issue_dispatch_run::worktree_keep_check`] asks, with each child
/// through the bounded runner under one deadline. A status that prints past
/// the cap is uncommitted changes; a listing that does, or any child that
/// does not answer in time, is a check that could not be made. Either keeps
/// the tree.
async fn bounded_keep_check(worktree: &Path, clone: &Path) -> Result<TreeKeepCheck, String> {
    let (program, bound) = keep_check_runner(worktree);
    let program = program.as_deref();
    let deadline = Deadline::after(bound);
    let status = async {
        match keep_check_child(program, worktree, &["status", "--porcelain"], deadline).await {
            Ok(out) => Ok(!String::from_utf8_lossy(&out).trim().is_empty()),
            Err(e) if crate::issue_dispatch_run::is_subprocess_overflow(&e) => Ok(true),
            Err(e) => Err(e),
        }
    };
    let (dirty, nested) = tokio::join!(status, bounded_nested(worktree, clone, program, deadline));
    crate::issue_dispatch_run::keep_check_answer(dirty, nested)
}

/// The re-validation just before `git worktree remove`, run under the
/// repository's worktree lock: whether the directory at the reclaimed path
/// is still the worktree the reclaim checked. In order: the same canonical
/// path and device and inode; a linked worktree of the same repository with
/// its owner marker in the same git metadata directory, naming the item's
/// launch; the item's branch
/// checked out; no other worktree inside it (asked again, since one may have
/// been added since the keep check); and, last, the same device and inode
/// again, since the probes in between take time. Any difference keeps the
/// tree: [`RemovalGuard::Changed`], or [`RemovalGuard::Keep`] naming the
/// nested worktrees (or that they could not be asked for).
async fn still_the_checked_worktree(checked: Checked) -> RemovalGuard {
    #[cfg(test)]
    seams::before_revalidate(&checked.worktree);
    let deadline = Deadline::after(REVALIDATE_TIMEOUT);
    if !still_the_same_directory(&checked).await {
        return RemovalGuard::Changed;
    }
    #[cfg(test)]
    seams::after_first_identity(&checked.worktree);
    let Some(facts) = probe::worktree_facts(&checked.worktree, deadline).await else {
        return RemovalGuard::Changed;
    };
    if facts.geometry.toplevel != checked.worktree
        || facts.geometry.main != checked.clone
        || facts.git_dir != checked.git_dir
    {
        return RemovalGuard::Changed;
    }
    // The marker again, as it is now: a later launch that created the tree
    // again since the first check names itself there (PR #2431 round 2).
    let marked = marked_launch(&facts.git_dir).await == MarkedLaunch::Names(checked.launch.clone());
    if !marked
        || probe::head_branch(&checked.worktree, deadline)
            .await
            .as_deref()
            != Some(checked.branch.as_str())
    {
        return RemovalGuard::Changed;
    }
    let (program, _) = keep_check_runner(&checked.worktree);
    match bounded_nested(
        &checked.worktree,
        &checked.clone,
        program.as_deref(),
        deadline,
    )
    .await
    {
        Ok(nested) if !nested.is_empty() => {
            return RemovalGuard::Keep(WorktreeRemoval::KeptHoldsWorktrees(nested));
        }
        Ok(_) => {}
        Err(_) => return RemovalGuard::Keep(WorktreeRemoval::KeptCouldNotCheck),
    }
    // The last check, as close to the removal as this daemon can make it.
    if !still_the_same_directory(&checked).await {
        return RemovalGuard::Changed;
    }
    RemovalGuard::Proceed
}

/// Reclaim the item `id` names: remove its worktree, keep its branch. With
/// `dry_run`, report the verdict it would reach and remove nothing.
pub async fn reclaim(
    registry: &Arc<AgentPtyRegistry>,
    id: String,
    dry_run: bool,
    caller: Option<&CallerClaim>,
) -> ReclaimOutcome {
    reclaim_within(registry, id, dry_run, caller, RECLAIM_TIMEOUT).await
}

/// [`reclaim`], answering after at most `bound` of removal.
async fn reclaim_within(
    registry: &Arc<AgentPtyRegistry>,
    id: String,
    dry_run: bool,
    caller: Option<&CallerClaim>,
    bound: Duration,
) -> ReclaimOutcome {
    reclaim_with(registry, id, dry_run, caller, bound, RECLAIM_LOCK_WAIT).await
}

/// [`reclaim_within`], waiting at most `lock_wait` for the repository's
/// worktree lock ([`RECLAIM_LOCK_WAIT`] in production).
async fn reclaim_with(
    registry: &Arc<AgentPtyRegistry>,
    id: String,
    dry_run: bool,
    caller: Option<&CallerClaim>,
    bound: Duration,
    lock_wait: Duration,
) -> ReclaimOutcome {
    // Any claim, well formed or not: see the module's "Who may act".
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
        journal,
        works,
        raw,
        worktree,
        identity,
    } = prepared;
    let shown_path = worktree.to_string_lossy().into_owned();
    let refuse = |reason| {
        let mut outcome = ReclaimOutcome::refused(Some(item.id.to_string()), dry_run, reason);
        outcome.worktree = Some(shown_path.clone());
        outcome.branch = item.branch.clone();
        outcome
    };

    // The Git checks, each child bounded.
    let deadline = Deadline::after(RECLAIM_CHECK_TIMEOUT);
    let facts = match probe::worktree_facts(&worktree, deadline).await {
        Some(facts) if facts.geometry.toplevel == worktree && facts.geometry.main != worktree => {
            facts
        }
        _ => return refuse(RefusalReason::NotLinkedWorktree),
    };
    let Some(branch) = item.branch.clone() else {
        return refuse(RefusalReason::BranchMismatch);
    };
    // The marker must name this item's launch (PR #2431 round 2): the
    // branch and the marker's presence are the same for a later run that
    // created the tree again at this path, and the journal's record of such
    // a run may be missing, so the decision rests on what the tree itself
    // says created it, never on the absence of another run.
    match marked_launch(&facts.git_dir).await {
        MarkedLaunch::NotMarked => return refuse(RefusalReason::NotOwned),
        MarkedLaunch::Names(launch) if launch == item.id => {}
        MarkedLaunch::Names(_) => return refuse(RefusalReason::NewerRun),
        MarkedLaunch::Unnamed => return refuse(RefusalReason::OwnerUnconfirmed),
    }
    if probe::head_branch(&worktree, deadline).await.as_deref() != Some(branch.as_str()) {
        return refuse(RefusalReason::BranchMismatch);
    }
    let unpushed_commits = probe::unpushed_commits(&worktree, deadline).await;
    let checked = Checked {
        raw: raw.clone(),
        worktree: worktree.clone(),
        identity,
        clone: facts.geometry.main.clone(),
        git_dir: facts.git_dir.clone(),
        branch: branch.clone(),
        launch: item.id.clone(),
    };

    // The hold, the removal and its record run in their own task, as
    // `close`'s cleanup does: a removal that outlives the bound keeps its
    // hold until it ends, and records itself when it succeeds.
    let task = {
        let registry = Arc::clone(registry);
        tokio::spawn(async move {
            let worktree = checked.worktree.clone();
            let clone = checked.clone.clone();
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
            match in_use_by_identity(&registry, &worktree).await {
                InUse::No => {}
                InUse::Yes => return Removal::InUse,
                InUse::Unknown => return Removal::InUseUnknown,
            }
            if dry_run {
                return match bounded_keep_check(&worktree, &clone).await {
                    Ok(TreeKeepCheck::Clean) => Removal::done(ReclaimVerdict::WouldRemove),
                    Ok(TreeKeepCheck::Dirty) => Removal::done(ReclaimVerdict::KeptDirty),
                    Ok(TreeKeepCheck::HoldsWorktrees(nested)) => Removal::Done {
                        verdict: ReclaimVerdict::KeptHoldsWorktrees,
                        nested,
                        recorded: false,
                    },
                    Err(_) => Removal::done(ReclaimVerdict::KeptCouldNotCheck),
                };
            }
            // The repository's worktree lock, held across the keep check,
            // the re-validation and the removal, so the deck's own worktree
            // creation and removal in this repository wait for it. Without
            // it nothing is removed.
            let _repo_lock =
                match crate::issue_dispatch_run::acquire_worktree_lock_within(&clone, lock_wait)
                    .await
                {
                    Ok(lock) => lock,
                    Err(WorktreeLockUnavailable::Busy) => {
                        return Removal::done(ReclaimVerdict::KeptBusy);
                    }
                    Err(WorktreeLockUnavailable::Failed) => {
                        return Removal::done(ReclaimVerdict::KeptCouldNotLock);
                    }
                };
            let removal = crate::issue_dispatch_run::remove_worktree_outcome_checked(
                &worktree,
                &clone,
                RemovalPolicy::KeepIfDirty,
                || bounded_keep_check(&worktree, &clone),
                || still_the_checked_worktree(checked),
            )
            .await;
            match removal {
                None => Removal::done(ReclaimVerdict::KeptChanged),
                Some(WorktreeRemoval::Removed) => {
                    #[cfg(test)]
                    seams::after_removal(&worktree);
                    let recorded = tokio::task::spawn_blocking(move || {
                        close_all(&journal, &works, CloseHow::Reclaimed)
                    })
                    .await
                    .unwrap_or(false);
                    Removal::Done {
                        verdict: ReclaimVerdict::Removed,
                        nested: Vec::new(),
                        recorded,
                    }
                }
                Some(WorktreeRemoval::KeptDirty) => Removal::done(ReclaimVerdict::KeptDirty),
                Some(WorktreeRemoval::KeptCouldNotCheck) => {
                    Removal::done(ReclaimVerdict::KeptCouldNotCheck)
                }
                Some(WorktreeRemoval::RemoveFailed) => Removal::done(ReclaimVerdict::RemoveFailed),
                Some(WorktreeRemoval::KeptHoldsWorktrees(nested)) => Removal::Done {
                    verdict: ReclaimVerdict::KeptHoldsWorktrees,
                    nested,
                    recorded: false,
                },
            }
        })
    };
    let (verdict, nested_worktrees, recorded) = match tokio::time::timeout(bound, task).await {
        Ok(Ok(Removal::Done {
            verdict,
            nested,
            recorded,
        })) => (verdict, nested, recorded),
        Ok(Ok(Removal::InUse)) => return refuse(RefusalReason::StillInUse),
        Ok(Ok(Removal::InUseUnknown)) => return refuse(RefusalReason::InUseUnknown),
        Ok(Err(join_error)) => {
            tracing::warn!(error = %join_error, "interrupted work: the reclaim task failed");
            (ReclaimVerdict::RemoveFailed, Vec::new(), false)
        }
        Err(_) => (ReclaimVerdict::TimedOut, Vec::new(), false),
    };
    let message = if verdict == ReclaimVerdict::Removed && !recorded {
        crate::interrupted_work::RECLAIM_NOT_RECORDED.to_string()
    } else {
        reclaim_sentence(verdict).to_string()
    };
    ReclaimOutcome {
        id: Some(item.id.to_string()),
        dry_run,
        verdict,
        message,
        refusal: None,
        worktree: Some(shown_path),
        branch: Some(branch),
        unpushed_commits,
        nested_worktrees,
        recorded,
    }
}

/// Whether a dismissal's selector is within [`MAX_DISMISS_IDS`],
/// [`MAX_DISMISS_ID_BYTES`] and [`MAX_DISMISS_BYTES`].
fn selector_fits(ids: &[String]) -> bool {
    ids.len() <= MAX_DISMISS_IDS
        && ids.iter().all(|id| id.len() <= MAX_DISMISS_ID_BYTES)
        && ids.iter().map(String::len).sum::<usize>() <= MAX_DISMISS_BYTES
}

/// Dismiss the items `ids` name, or every listed item with `all`.
pub async fn dismiss(
    registry: &AgentPtyRegistry,
    ids: Vec<String>,
    all: bool,
    caller: Option<&CallerClaim>,
) -> DismissOutcome {
    let refused = |reason| DismissOutcome {
        refusal: Some(crate::interrupted_work::Refusal::new(reason)),
        ..DismissOutcome::default()
    };
    // Any claim, well formed or not: see the module's "Who may act".
    if caller.is_some() {
        return refused(RefusalReason::AgentCaller);
    }
    if all == !ids.is_empty() {
        return refused(RefusalReason::BadRequest);
    }
    if !selector_fits(&ids) {
        return refused(RefusalReason::TooLarge);
    }
    let answer = blocking(registry, move |journal, retained| {
        let Ok(snap) = snapshot(&journal, retained.as_deref()) else {
            return refused(RefusalReason::Unreadable);
        };
        // A truncated journal read may have left out works under an item, and
        // a dismissal closes the works it read: refused, as reclaim is, rather
        // than leaving those works unclosed (docs review R2).
        if snap.read.stats.truncated {
            return refused(RefusalReason::IncompleteRead);
        }
        let mut outcome = DismissOutcome {
            incomplete: snap.list.incomplete,
            ..DismissOutcome::default()
        };
        let mut targets: Vec<&InterruptedItem> = Vec::new();
        if all {
            targets.extend(snap.list.items.iter());
        } else {
            let mut seen = std::collections::HashSet::new();
            for id in &ids {
                match resolve(&snap.list, id) {
                    Resolved::One(item) => {
                        if seen.insert(&item.id) {
                            targets.push(item);
                        }
                    }
                    Resolved::NotListed => {
                        outcome.push_refused(id.clone(), RefusalReason::NotListed)
                    }
                    Resolved::Ambiguous => {
                        outcome.push_refused(id.clone(), RefusalReason::Ambiguous)
                    }
                }
            }
        }
        let sets = CloseSets::build(&snap.read);
        for item in targets {
            if close_all(&journal, &sets.works_under(&item.id), CloseHow::Dismissed) {
                outcome.push_dismissed(DismissedItem {
                    id: item.id.to_string(),
                    name: item.name.clone(),
                });
            } else {
                outcome.push_refused(item.id.to_string(), RefusalReason::NotRecorded);
            }
        }
        outcome.fit(MAX_REPLY_BYTES);
        outcome
    })
    .await;
    answer.unwrap_or_else(|_| refused(RefusalReason::Unreadable))
}

/// Write `closed how=<how>` on every work, waiting for each write; `true`
/// when all were written. Blocking.
fn close_all(journal: &WorkJournal, works: &[WorkId], how: CloseHow) -> bool {
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

/// One item's Git facts, its probes bounded by `deadline` and its pull
/// request lookup by `pr_deadline`. A plain agent's directory is not a
/// worktree the deck made, so only whether it exists is read for it.
async fn git_facts(
    kind: ItemKind,
    worktree: Option<String>,
    branch: Option<String>,
    gh: &OsStr,
    deadline: Deadline,
    pr_deadline: Deadline,
) -> GitFacts {
    let Some(worktree) = worktree else {
        return GitFacts::default();
    };
    let path = PathBuf::from(worktree);
    let lookup = async {
        #[cfg(test)]
        if let Some(stall) = seams::metadata_stall(&path) {
            tokio::time::sleep(stall).await;
        }
        tokio::fs::metadata(&path).await
    };
    // Bounded like the probes (PR #2431 review): a lookup on a stalled mount
    // answers "not known" at the deadline instead of holding the list. The
    // blocking-pool thread it runs on is not interrupted, only waited for no
    // longer.
    let worktree_exists = match deadline.remaining() {
        Some(left) => match tokio::time::timeout(left, lookup).await {
            Ok(Ok(meta)) => Some(meta.is_dir()),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
            Ok(Err(_)) | Err(_) => None,
        },
        None => None,
    };
    let mut facts = GitFacts {
        worktree_exists,
        ..GitFacts::default()
    };
    if worktree_exists != Some(true) || kind == ItemKind::Agent {
        return facts;
    }
    let branch = branch.as_deref();
    let exists = async {
        match branch {
            Some(branch) => probe::branch_exists(&path, branch, deadline).await,
            None => None,
        }
    };
    let pr = async {
        match branch {
            Some(branch) => probe::pr_state(&path, branch, gh, pr_deadline).await,
            None => PrFact::Unknown,
        }
    };
    let (dirty, unpushed, exists, pr) = tokio::join!(
        probe::is_dirty(&path, deadline),
        probe::unpushed_commits(&path, deadline),
        exists,
        pr
    );
    facts.dirty = dirty;
    facts.unpushed_commits = unpushed;
    facts.branch_exists = exists;
    facts.pr = pr;
    facts
}

/// Fill each item's Git facts, [`GIT_FACT_CONCURRENCY`] items at a time, all
/// by `total`: an item that has not started by then keeps the empty facts.
async fn add_git_facts(items: &mut [InterruptedItem], gh: &OsStr, total: Deadline) {
    let permits = Arc::new(tokio::sync::Semaphore::new(GIT_FACT_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for (at, item) in items.iter().enumerate() {
        let (kind, worktree, branch) = (item.kind, item.worktree.clone(), item.branch.clone());
        let permits = Arc::clone(&permits);
        let gh = gh.to_os_string();
        set.spawn(async move {
            let wait = total.remaining()?;
            let _permit = tokio::time::timeout(wait, permits.acquire_owned())
                .await
                .ok()?
                .ok()?;
            total.remaining()?;
            let deadline = Deadline::after(GIT_FACT_TIMEOUT).min(total);
            let pr_deadline = Deadline::after(PR_STATE_TIMEOUT).min(total);
            Some((
                at,
                git_facts(kind, worktree, branch, &gh, deadline, pr_deadline).await,
            ))
        });
    }
    for item in items.iter_mut() {
        item.git = Some(GitFacts::default());
    }
    while let Some(joined) = set.join_next().await {
        if let Ok(Some((at, facts))) = joined
            && let Some(item) = items.get_mut(at)
        {
            item.git = Some(facts);
        }
    }
}

/// Test seams: hooks a test arms for one worktree path. Every seam is off
/// unless a test arms it, and is keyed by the path so tests running at once
/// cannot fire each other's.
#[cfg(test)]
mod seams {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};

    type Hook = Box<dyn FnOnce() + Send>;

    static BEFORE_REVALIDATE: LazyLock<Mutex<HashMap<PathBuf, Hook>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static AFTER_REMOVAL: LazyLock<Mutex<HashMap<PathBuf, Hook>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static AFTER_FIRST_IDENTITY: LazyLock<Mutex<HashMap<PathBuf, Hook>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn take(seam: &Mutex<HashMap<PathBuf, Hook>>, worktree: &Path) -> Option<Hook> {
        seam.lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(worktree)
    }

    /// Run `hook` once, between a reclaim's keep check and its
    /// re-validation, for the reclaim of `worktree` (canonical).
    pub(super) fn arm_before_revalidate(worktree: &Path, hook: impl FnOnce() + Send + 'static) {
        BEFORE_REVALIDATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(worktree.to_path_buf(), Box::new(hook));
    }

    /// Run `hook` once, right after the re-validation's first identity
    /// check of `worktree` (canonical) passed.
    pub(super) fn arm_after_first_identity(worktree: &Path, hook: impl FnOnce() + Send + 'static) {
        AFTER_FIRST_IDENTITY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(worktree.to_path_buf(), Box::new(hook));
    }

    pub(super) fn after_first_identity(worktree: &Path) {
        if let Some(hook) = take(&AFTER_FIRST_IDENTITY, worktree) {
            hook();
        }
    }

    /// Run `hook` once, after `worktree`'s removal and before it is
    /// recorded.
    pub(super) fn arm_after_removal(worktree: &Path, hook: impl FnOnce() + Send + 'static) {
        AFTER_REMOVAL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(worktree.to_path_buf(), Box::new(hook));
    }

    static METADATA_STALL: LazyLock<Mutex<HashMap<PathBuf, std::time::Duration>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// PR #2431 review (Qodo): make the list's look at whether `worktree`
    /// exists take `stall` first, as a lookup on a stalled mount would.
    pub(super) fn arm_metadata_stall(worktree: &Path, stall: std::time::Duration) {
        METADATA_STALL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(worktree.to_path_buf(), stall);
    }

    pub(super) fn metadata_stall(worktree: &Path) -> Option<std::time::Duration> {
        METADATA_STALL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(worktree)
            .copied()
    }

    static KEEP_CHECK: LazyLock<Mutex<HashMap<PathBuf, (PathBuf, std::time::Duration)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Until [`disarm_keep_check`], run the keep check's `git status` and
    /// `git worktree list` for the reclaim of `worktree` (canonical) as
    /// `program`, bounded by `deadline` from the check's start rather than
    /// [`super::RECLAIM_CHECK_TIMEOUT`].
    pub(super) fn arm_keep_check(worktree: &Path, program: PathBuf, deadline: std::time::Duration) {
        KEEP_CHECK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(worktree.to_path_buf(), (program, deadline));
    }

    pub(super) fn disarm_keep_check(worktree: &Path) {
        KEEP_CHECK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(worktree);
    }

    /// The program and deadline armed for `worktree`'s keep check.
    pub(super) fn keep_check(worktree: &Path) -> Option<(PathBuf, std::time::Duration)> {
        KEEP_CHECK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(worktree)
            .cloned()
    }

    pub(super) fn before_revalidate(worktree: &Path) {
        if let Some(hook) = take(&BEFORE_REVALIDATE, worktree) {
            hook();
        }
    }

    pub(super) fn after_removal(worktree: &Path) {
        if let Some(hook) = take(&AFTER_REMOVAL, worktree) {
            hook();
        }
    }
}

#[cfg(all(test, unix))]
#[path = "interrupted_work_daemon_tests.rs"]
mod tests;
