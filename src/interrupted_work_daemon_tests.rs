//! PRD #1790 stage 2: the interrupted work verbs' handlers, against a real
//! journal in a temporary state directory and, for reclaim, a real Git
//! repository with a linked worktree.
//!
//! These run `git` in the fast tier for the reason `issue_dispatch_run`'s
//! worktree tests do: what is under test is what Git answers about a real
//! worktree (its branch, its dirt, whether `git worktree remove` removed it),
//! and a stand-in would test the stand-in. Every Git call is sandboxed by
//! `git_env::fixture_git` (no global or system config, a private `HOME`), and
//! nothing reaches the network: no repository has a remote, so the pull
//! request lookup is never asked for.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::agent_pty::AgentPtyRegistry;
use crate::deck_identity::DeckId;
use crate::interrupted_work::{
    DismissOutcome, InterruptedList, PrFact, ReclaimVerdict, RefusalReason,
};
use crate::work_journal::{
    AttemptId, Budget, Budgets, EndHow, JournalOptions, Launch, Refs, WorkId,
};

const RUN: &str = "r1";

fn deck() -> DeckId {
    DeckId::parse("abcdefgh").unwrap()
}

/// A registry whose work tracker writes to a journal under `state`.
fn registry_with_journal(state: &Path, options: JournalOptions) -> Arc<AgentPtyRegistry> {
    std::fs::create_dir_all(state).unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    let journal =
        WorkJournal::try_open_with(state, RUN, &deck(), options).expect("open the journal");
    registry.work().install(journal);
    registry
}

fn journal(registry: &AgentPtyRegistry) -> &WorkJournal {
    registry.work().journal().expect("installed")
}

fn record(registry: &AgentPtyRegistry, event: Event) {
    journal(registry).try_record(event).expect("written");
}

/// Record a work that ran under `launch` (a child of `parent`) and whose one
/// attempt a restart interrupted.
fn interrupted_work(
    registry: &AgentPtyRegistry,
    agent: u64,
    launch: Launch,
    parent: Option<&WorkId>,
    branch: Option<&str>,
    workspace: Option<&Path>,
) -> WorkId {
    let journal = journal(registry);
    let work = journal.mint_work_id().unwrap();
    let attempt = AttemptId::new(RUN, agent).unwrap();
    record(
        registry,
        Event::Opened {
            work: work.clone(),
            launch,
            parent: parent.cloned(),
            refs: Box::new(Refs {
                branch: branch.map(str::to_string),
                ..Refs::default()
            }),
            workspace: workspace.and_then(|path| journal.workspace(path)),
        },
    );
    record(
        registry,
        Event::AttemptStarted {
            work: work.clone(),
            attempt: attempt.clone(),
            pane: format!("pane-{agent}"),
            agent,
            role: None,
        },
    );
    record(
        registry,
        Event::AttemptEnded {
            work: work.clone(),
            attempt,
            how: EndHow::Interrupted,
            code: None,
            cause: None,
        },
    );
    work
}

/// A dispatched unit with an orchestration under it, both interrupted.
fn unit(registry: &AgentPtyRegistry, branch: &str, worktree: Option<&Path>) -> (WorkId, WorkId) {
    let root = interrupted_work(registry, 1, Launch::Dispatch, None, Some(branch), worktree);
    let child = interrupted_work(
        registry,
        2,
        Launch::Orchestration,
        Some(&root),
        None,
        worktree,
    );
    (root, child)
}

fn closes(registry: &AgentPtyRegistry) -> Vec<(WorkId, CloseHow)> {
    journal(registry)
        .read()
        .unwrap()
        .lines
        .into_iter()
        .filter_map(|line| match line.event {
            Event::Closed { work, how } => Some((work, how)),
            _ => None,
        })
        .collect()
}

async fn listed(registry: &AgentPtyRegistry) -> InterruptedList {
    list(registry, false).await.expect("listed")
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn claim() -> CallerClaim {
    CallerClaim {
        pane_id: "p1".into(),
        token: "t".into(),
    }
}

/// A repository at `<root>/repo` with a linked worktree at `<root>/wt` on
/// branch `agent/dispatch-x`, both canonical.
fn repo_with_worktree(root: &Path) -> (PathBuf, PathBuf) {
    let repo = root.join("repo");
    let wt = root.join("wt");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let out = crate::git_env::fixture_git(dir, root)
            .args(args)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    };
    git(&repo, &["init", "-q", "."]);
    crate::worktree_owner::pin_fixture_eol(&repo, root);
    std::fs::write(repo.join("a.txt"), "hi").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent/dispatch-x",
            &wt.to_string_lossy(),
        ],
    );
    (
        std::fs::canonicalize(repo).unwrap(),
        std::fs::canonicalize(wt).unwrap(),
    )
}

/// A marker as a build before the launch field wrote it: it names who
/// created the worktree, not which launch.
fn mark(wt: &Path) {
    crate::worktree_owner::write_marker(
        wt,
        "agent/dispatch-x",
        &crate::worktree_owner::Creator::dispatch("x"),
    )
    .expect("marker written");
}

/// The marker a dispatch writes when it creates `wt` for the launch
/// `launch` (PR #2431 round 2): the document with `creator.launch` naming
/// that launch's work. Written as the bytes a marker holds, so the fixture
/// does not depend on the writer it checks.
fn mark_naming(wt: &Path, launch: &WorkId) {
    let path = crate::worktree_owner::marker_path(wt).expect("the marker's directory resolves");
    let doc = serde_json::json!({
        "schema": 1,
        "created_by": "dot-agent-deck",
        "version": "test",
        "created_at": "2026-11-20T00:00:00Z",
        "pid": 1,
        "creator": { "kind": "dispatch", "subject": "x", "launch": launch.as_str() },
        "branch": "agent/dispatch-x",
    });
    std::fs::write(&path, format!("{doc:#}\n")).expect("marker written");
}

fn branch_exists_in(repo: &Path, root: &Path, branch: &str) -> bool {
    let out = crate::git_env::fixture_git(repo, root)
        .args(["branch", "--list", branch])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[test]
fn dismiss_writes_closed_dismissed_on_every_work_of_the_item_and_hides_it() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let (root, child) = unit(&registry, "agent/dispatch-x", None);
        let other = interrupted_work(&registry, 3, Launch::Pane, None, None, None);
        assert_eq!(listed(&registry).await.items.len(), 2);

        let outcome = dismiss(&registry, vec![root.to_string()], false, None).await;
        assert_eq!(outcome.refusal, None);
        assert!(outcome.refused.is_empty(), "{outcome:?}");
        assert_eq!(outcome.dismissed.len(), 1);
        assert_eq!(outcome.dismissed[0].id, root.to_string());
        assert_eq!(
            closes(&registry),
            vec![
                (root.clone(), CloseHow::Dismissed),
                (child, CloseHow::Dismissed)
            ]
        );
        let after = listed(&registry).await;
        assert_eq!(
            after.items.iter().map(|i| &i.id).collect::<Vec<_>>(),
            vec![&other]
        );

        // An id no item has is refused by name; the rest still go.
        let outcome = dismiss(
            &registry,
            vec!["no-such-item".into(), other.to_string()],
            false,
            None,
        )
        .await;
        assert_eq!(outcome.dismissed.len(), 1);
        assert_eq!(outcome.refused.len(), 1);
        assert_eq!(outcome.refused[0].id, "no-such-item");
        assert_eq!(outcome.refused[0].refusal.reason, RefusalReason::NotListed);
        assert!(listed(&registry).await.items.is_empty());
    });
}

#[test]
fn dismiss_all_hides_every_item_and_a_bad_or_agent_request_writes_nothing() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        unit(&registry, "agent/dispatch-x", None);
        interrupted_work(&registry, 3, Launch::Pane, None, None, None);

        let refused_whole = |outcome: &DismissOutcome| outcome.refusal.as_ref().map(|r| r.reason);
        let agent = dismiss(&registry, Vec::new(), true, Some(&claim())).await;
        assert_eq!(refused_whole(&agent), Some(RefusalReason::AgentCaller));
        let neither = dismiss(&registry, Vec::new(), false, None).await;
        assert_eq!(refused_whole(&neither), Some(RefusalReason::BadRequest));
        let both = dismiss(&registry, vec!["x".into()], true, None).await;
        assert_eq!(refused_whole(&both), Some(RefusalReason::BadRequest));
        assert!(closes(&registry).is_empty());

        let all = dismiss(&registry, Vec::new(), true, None).await;
        assert_eq!(all.refusal, None);
        assert_eq!(all.dismissed.len(), 2);
        assert!(listed(&registry).await.items.is_empty());
    });
}

#[test]
fn reclaim_refuses_an_agent_caller_a_plain_agent_and_an_unknown_id() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let pane = interrupted_work(&registry, 3, Launch::Pane, None, None, Some(state.path()));
        let reason = |o: &ReclaimOutcome| o.refusal.as_ref().map(|r| r.reason);

        let agent = reclaim(&registry, pane.to_string(), false, Some(&claim())).await;
        assert_eq!(agent.verdict, ReclaimVerdict::Refused);
        assert_eq!(reason(&agent), Some(RefusalReason::AgentCaller));

        let plain = reclaim(&registry, pane.to_string(), false, None).await;
        assert_eq!(reason(&plain), Some(RefusalReason::PlainAgent));
        assert_eq!(
            plain.message,
            crate::interrupted_work::REFUSED_PLAIN_AGENT,
            "the sentence comes from the one wording place"
        );

        let unknown = reclaim(&registry, "nothing".into(), true, None).await;
        assert_eq!(reason(&unknown), Some(RefusalReason::NotListed));
        assert!(closes(&registry).is_empty());
    });
}

#[test]
fn reclaim_refuses_a_truncated_journal_read() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let defaults = Budgets::default();
        let options = JournalOptions {
            budgets: Budgets {
                read: Budget {
                    entries: 4,
                    ..defaults.read
                },
                ..defaults
            },
            ..JournalOptions::default()
        };
        let registry = registry_with_journal(state.path(), options);
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(state.path()));
        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(
            outcome.refusal.map(|r| r.reason),
            Some(RefusalReason::IncompleteRead)
        );
    });
}

/// R2 (docs review): a truncated journal read can omit an item's
/// descendants, so a dismissal then would close the root and leave works
/// under it unclosed. Dismiss refuses it, as reclaim does, for one id and for
/// Dismiss all, and writes no close.
#[test]
fn dismiss_refuses_a_truncated_journal_read_and_writes_no_close() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let defaults = Budgets::default();
        let options = JournalOptions {
            budgets: Budgets {
                read: Budget {
                    entries: 4,
                    ..defaults.read
                },
                ..defaults
            },
            ..JournalOptions::default()
        };
        let registry = registry_with_journal(state.path(), options);
        let (root, _) = unit(&registry, "agent/dispatch-x", None);
        let whole = |o: &DismissOutcome| o.refusal.as_ref().map(|r| r.reason);

        let one = dismiss(&registry, vec![root.to_string()], false, None).await;
        assert_eq!(whole(&one), Some(RefusalReason::IncompleteRead), "{one:?}");
        assert!(one.dismissed.is_empty(), "{one:?}");

        let all = dismiss(&registry, Vec::new(), true, None).await;
        assert_eq!(whole(&all), Some(RefusalReason::IncompleteRead), "{all:?}");
        assert!(all.dismissed.is_empty(), "{all:?}");

        assert!(closes(&registry).is_empty(), "no close was written");
    });
}

#[test]
fn reclaim_refuses_a_worktree_without_the_owner_marker_or_on_another_branch() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        let state = root_dir.join("state");
        let registry = registry_with_journal(&state, JournalOptions::default());
        let reason = |o: &ReclaimOutcome| o.refusal.as_ref().map(|r| r.reason);

        // Not marked: the deck did not create it.
        let (unmarked, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        let outcome = reclaim(&registry, unmarked.to_string(), false, None).await;
        assert_eq!(
            reason(&outcome),
            Some(RefusalReason::NotOwned),
            "{outcome:?}"
        );
        dismiss(&registry, vec![unmarked.to_string()], false, None).await;

        // Marked, but the journal names another branch than the one checked
        // out.
        let (other_branch, _) = unit(&registry, "agent/dispatch-y", Some(&wt));
        mark_naming(&wt, &other_branch);
        let outcome = reclaim(&registry, other_branch.to_string(), false, None).await;
        assert_eq!(
            reason(&outcome),
            Some(RefusalReason::BranchMismatch),
            "{outcome:?}"
        );
        dismiss(&registry, vec![other_branch.to_string()], false, None).await;

        // The main checkout is not a linked worktree.
        let (main, _) = unit(&registry, "agent/dispatch-x", Some(&repo));
        let outcome = reclaim(&registry, main.to_string(), false, None).await;
        assert_eq!(
            reason(&outcome),
            Some(RefusalReason::NotLinkedWorktree),
            "{outcome:?}"
        );

        assert!(wt.is_dir(), "nothing was removed");
    });
}

#[test]
fn reclaim_keeps_a_dirty_worktree_and_leaves_the_item_listed() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        std::fs::write(wt.join("scratch.txt"), "work").unwrap();
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);

        let dry = reclaim(&registry, root.to_string(), true, None).await;
        assert_eq!(dry.verdict, ReclaimVerdict::KeptDirty, "{dry:?}");
        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptDirty, "{outcome:?}");
        assert_eq!(outcome.message, crate::interrupted_work::RECLAIM_KEPT_DIRTY);
        assert!(!outcome.recorded);
        assert!(wt.join("scratch.txt").is_file());
        assert!(closes(&registry).is_empty());
        assert_eq!(listed(&registry).await.items.len(), 1);
    });
}

#[test]
fn reclaim_removes_a_clean_owned_worktree_keeps_its_branch_and_records_it() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        // One commit no remote holds: counted, never blocking (D5).
        std::fs::write(wt.join("b.txt"), "b").unwrap();
        for args in [&["add", "."][..], &["commit", "-qm", "b"][..]] {
            let out = crate::git_env::fixture_git(&wt, &root_dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, child) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);

        let git_list = list(&registry, true).await.unwrap();
        let facts = git_list.items[0].git.clone().expect("asked for");
        assert_eq!(facts.worktree_exists, Some(true));
        assert_eq!(facts.dirty, Some(false));
        assert_eq!(facts.branch_exists, Some(true));
        assert_eq!(facts.unpushed_commits, Some(2), "{facts:?}");
        assert_eq!(facts.pr, PrFact::Unknown, "no remote, so no PR state");

        let dry = reclaim(&registry, root.to_string(), true, None).await;
        assert_eq!(dry.verdict, ReclaimVerdict::WouldRemove, "{dry:?}");
        assert!(wt.is_dir(), "a dry run removes nothing");
        assert!(closes(&registry).is_empty());

        // By branch, as a person would name it.
        let outcome = reclaim(&registry, "agent/dispatch-x".into(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::Removed, "{outcome:?}");
        assert_eq!(outcome.id.as_deref(), Some(root.as_str()));
        assert_eq!(outcome.unpushed_commits, Some(2));
        assert!(outcome.recorded);
        assert_eq!(outcome.message, crate::interrupted_work::RECLAIM_REMOVED);
        assert!(!wt.exists(), "the worktree is gone");
        assert!(
            branch_exists_in(&repo, &root_dir, "agent/dispatch-x"),
            "the branch is kept"
        );
        assert_eq!(
            closes(&registry),
            vec![(root, CloseHow::Reclaimed), (child, CloseHow::Reclaimed)]
        );
        assert!(listed(&registry).await.items.is_empty());
    });
}

#[test]
fn reclaim_keeps_and_names_a_worktree_inside_the_items_worktree() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        let nested = wt.join("pr-worktree");
        let out = crate::git_env::fixture_git(&repo, &root_dir)
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "pr",
                &nested.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        // The nested worktree also makes the outer one read as dirty; the
        // nested check wins, so the report names it (issue #2285).
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(
            outcome.verdict,
            ReclaimVerdict::KeptHoldsWorktrees,
            "{outcome:?}"
        );
        assert_eq!(outcome.nested_worktrees.len(), 1, "{outcome:?}");
        assert!(nested.is_dir() && wt.is_dir());
        assert!(closes(&registry).is_empty());
    });
}

#[test]
fn show_answers_the_item_and_refuses_an_unknown_id() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", None);
        let shown = show(&registry, root.to_string(), None).await.unwrap();
        assert_eq!(shown.item.id, root);
        assert_eq!(shown.item.branch.as_deref(), Some("agent/dispatch-x"));
        assert!(shown.reports.is_empty());
        assert_eq!(
            show(&registry, "nothing".into(), None).await,
            Err(crate::interrupted_work::REFUSED_NOT_LISTED.to_string())
        );
    });
}

/// Record a work that ran under `launch` in `workspace` on `branch`, whose
/// one attempt exited and which closed `how`: a run that finished, so it is
/// not listed.
fn finished_work(
    registry: &AgentPtyRegistry,
    agent: u64,
    launch: Launch,
    branch: &str,
    workspace: &Path,
    how: CloseHow,
) -> WorkId {
    let journal = journal(registry);
    let work = journal.mint_work_id().unwrap();
    let attempt = AttemptId::new(RUN, agent).unwrap();
    record(
        registry,
        Event::Opened {
            work: work.clone(),
            launch,
            parent: None,
            refs: Box::new(Refs {
                branch: Some(branch.to_string()),
                ..Refs::default()
            }),
            workspace: journal.workspace(workspace),
        },
    );
    record(
        registry,
        Event::AttemptStarted {
            work: work.clone(),
            attempt: attempt.clone(),
            pane: format!("pane-{agent}"),
            agent,
            role: None,
        },
    );
    record(
        registry,
        Event::AttemptEnded {
            work: work.clone(),
            attempt,
            how: EndHow::Exited,
            code: Some(0),
            cause: None,
        },
    );
    record(
        registry,
        Event::Closed {
            work: work.clone(),
            how,
        },
    );
    work
}

/// PR #2431 review (Greptile P1): an interrupted unit's worktree was removed
/// and the issue dispatched again, at the same path and on the same branch,
/// so the newer run's worktree carries the deck's marker and that branch.
/// The marker names the launch that created the tree, the newer run's, so
/// once that run has finished, reclaiming the OLDER item keeps the tree and
/// says why, dry run or not, and records nothing; the item stays listed.
/// When the newer run is a dispatch that was itself interrupted, reclaiming
/// the newer item is still offered. A plain pane started later in the tree
/// does not count (`reclaim_refuses_while_a_pane_runs_in_the_worktree_through_a_symlink`
/// removes the tree once its panes are gone).
#[test]
fn reclaim_of_an_older_item_keeps_the_worktree_a_newer_run_uses() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        // Its schedule fired the issue again: an issue dispatch, which
        // created the tree again and marked it as its own.
        let newer_fire = finished_work(
            registry,
            7,
            Launch::IssueDispatch,
            "agent/dispatch-x",
            &fx.wt,
            CloseHow::FireDone,
        );
        mark_naming(&fx.wt, &newer_fire);

        fx.assert_older_is_refused(registry, &older).await;
        let listed_ids: Vec<WorkId> = listed(registry)
            .await
            .items
            .into_iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(
            listed_ids,
            vec![older.clone()],
            "the older item stays listed"
        );

        // A newer run that was itself interrupted: the older item is still
        // refused, and the newer one is the one reclaim acts on.
        let (newer, _) = unit(registry, "agent/dispatch-x", Some(&fx.wt));
        mark_naming(&fx.wt, &newer);
        let outcome = reclaim(registry, older.to_string(), true, None).await;
        assert_eq!(reason_of(&outcome), Some(RefusalReason::NewerRun));
        let outcome = reclaim(registry, newer.to_string(), true, None).await;
        assert_eq!(
            outcome.verdict,
            ReclaimVerdict::WouldRemove,
            "the newest run's item can be reclaimed: {outcome:?}"
        );
    });
}

/// What every newer-run regression starts from: a clean linked worktree
/// holding a newer run's committed work, so nothing but the ownership check
/// stands between it and the removal, and a journal under `state`.
struct NewerRunFixture {
    _tmp: tempfile::TempDir,
    root_dir: PathBuf,
    wt: PathBuf,
    registry: Arc<AgentPtyRegistry>,
}

impl NewerRunFixture {
    fn new() -> Self {
        Self::with_journal(|_| {})
    }

    /// The fixture, with `before_open` run on the state directory before the
    /// journal is opened (to plant another run's segment).
    fn with_journal(before_open: impl FnOnce(&Path)) -> Self {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        std::fs::write(wt.join(".notes"), "the newer run's notes").unwrap();
        git_in(&wt, &root_dir, &["add", "."]);
        git_in(&wt, &root_dir, &["commit", "-qm", "newer run"]);
        let state = root_dir.join("state");
        before_open(&state);
        let registry = registry_with_journal(&state, JournalOptions::default());
        NewerRunFixture {
            _tmp: tmp,
            root_dir,
            wt,
            registry,
        }
    }

    /// The older, interrupted unit in the worktree.
    fn older(&self, registry: &AgentPtyRegistry) -> WorkId {
        unit(registry, "agent/dispatch-x", Some(&self.wt)).0
    }

    /// Reclaiming `older`, dry run and not, is refused because another run
    /// created the tree, and the tree and its newer work are still there;
    /// nothing records a close of `older`.
    async fn assert_older_is_refused(&self, registry: &Arc<AgentPtyRegistry>, older: &WorkId) {
        for dry_run in [true, false] {
            let outcome = reclaim(registry, older.to_string(), dry_run, None).await;
            assert_eq!(
                reason_of(&outcome),
                Some(RefusalReason::NewerRun),
                "dry run {dry_run}: {outcome:?}"
            );
            assert_eq!(outcome.verdict, ReclaimVerdict::Refused);
            assert_eq!(
                outcome.message,
                crate::interrupted_work::REFUSED_NEWER_RUN,
                "the sentence says why it was kept"
            );
            assert_eq!(
                outcome.worktree.as_deref(),
                Some(&*self.wt.to_string_lossy())
            );
            assert!(
                self.wt.join(".notes").is_file(),
                "dry run {dry_run}: the newer run's tree is kept"
            );
        }
        assert!(closes(registry).iter().all(|(work, _)| work != older));
    }
}

/// Record the attempt of `work`, which exited, and its close: a newer run
/// that finished. Its `opened` is written only with `opened`, and names
/// `workspace` (which may be none).
fn finished_launch(
    registry: &AgentPtyRegistry,
    work: &WorkId,
    agent: u64,
    opened: bool,
    workspace: Option<crate::work_journal::Workspace>,
) {
    if opened {
        record(
            registry,
            Event::Opened {
                work: work.clone(),
                launch: Launch::IssueDispatch,
                parent: None,
                refs: Box::new(Refs {
                    branch: Some("agent/dispatch-x".to_string()),
                    ..Refs::default()
                }),
                workspace,
            },
        );
    }
    let attempt = AttemptId::new(RUN, agent).unwrap();
    record(
        registry,
        Event::AttemptStarted {
            work: work.clone(),
            attempt: attempt.clone(),
            pane: format!("pane-{agent}"),
            agent,
            role: None,
        },
    );
    record(
        registry,
        Event::AttemptEnded {
            work: work.clone(),
            attempt,
            how: EndHow::Exited,
            code: Some(0),
            cause: None,
        },
    );
    record(
        registry,
        Event::Closed {
            work: work.clone(),
            how: CloseHow::FireDone,
        },
    );
}

/// PR #2431 round 2, auditor A1: the journal is best effort, so the newer
/// run that created the tree again may have no `opened` line at all (a full
/// writer queue dropped it). The tree's marker names that run's launch, so
/// reclaiming the older item keeps the tree.
#[test]
fn reclaim_keeps_a_newer_runs_tree_whose_opened_line_was_dropped() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        let newer = journal(registry).mint_work_id().unwrap();
        finished_launch(registry, &newer, 7, false, None);
        mark_naming(&fx.wt, &newer);

        fx.assert_older_is_refused(registry, &older).await;
    });
}

/// PR #2431 round 2, auditor A1: the newer run's `opened` was written, but
/// with no workspace (the journal writes none for a path it cannot record).
/// The marker still names that run, so the older item's reclaim keeps it.
#[test]
fn reclaim_keeps_a_newer_runs_tree_whose_opened_line_names_no_workspace() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        let newer = journal(registry).mint_work_id().unwrap();
        finished_launch(registry, &newer, 7, true, None);
        mark_naming(&fx.wt, &newer);

        fx.assert_older_is_refused(registry, &older).await;
    });
}

/// PR #2431 round 2, auditor A2: the newer run recorded its workspace
/// through a symlinked parent that was removed afterwards, so its recorded
/// path no longer leads to the tree. The marker names the newer run, so the
/// older item's reclaim keeps the tree.
#[test]
fn reclaim_keeps_a_newer_runs_tree_recorded_through_a_removed_alias() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        let alias = fx.root_dir.join("alias");
        std::os::unix::fs::symlink(&fx.root_dir, &alias).unwrap();
        let through_alias = alias.join(fx.wt.file_name().unwrap());
        let workspace = journal(registry).workspace(&through_alias);
        assert!(
            workspace
                .as_ref()
                .is_some_and(|w| w.as_str().contains("/alias/")),
            "fixture precondition: the workspace keeps the alias: {workspace:?}"
        );
        let newer = journal(registry).mint_work_id().unwrap();
        finished_launch(registry, &newer, 7, true, workspace);
        std::fs::remove_file(&alias).unwrap();
        mark_naming(&fx.wt, &newer);

        fx.assert_older_is_refused(registry, &older).await;
    });
}

/// One line of the dead run `run`'s segment, at `seq` and `ts_ms`.
fn planted_line(run: &str, seq: u64, ts_ms: u64, event: Event) -> String {
    serde_json::to_string(&crate::work_journal::Line {
        v: crate::work_journal::SCHEMA_VERSION,
        seq,
        ts_ms,
        run: run.to_string(),
        event,
    })
    .unwrap()
}

/// PR #2431 round 2, auditor A3: the wall clock stepped back between the
/// older item's launch and the newer run's, so in one run the newer
/// `opened` has the higher `seq` and the lower `ts_ms`, and a read ordered
/// by time puts it first. The marker names the newer run, so the older
/// item's reclaim keeps the tree.
#[test]
fn reclaim_keeps_a_newer_runs_tree_when_the_clock_stepped_back() {
    rt().block_on(async {
        let older = WorkId::parse("w-abcdefgh-00000000000000a1").unwrap();
        let newer = WorkId::parse("w-abcdefgh-00000000000000b2").unwrap();
        let wt_slot: std::sync::Arc<std::sync::Mutex<Option<PathBuf>>> = Default::default();
        let slot = std::sync::Arc::clone(&wt_slot);
        let (planted_older, planted_newer) = (older.clone(), newer.clone());
        let fx = NewerRunFixture::with_journal(move |state| {
            let (older, newer) = (planted_older, planted_newer);
            let wt = state.parent().unwrap().join("wt");
            let workspace = crate::work_journal::Workspace::from_path(&wt);
            *slot.lock().unwrap() = Some(wt);
            let run = "r0";
            let opened = |work: &WorkId, launch| Event::Opened {
                work: work.clone(),
                launch,
                parent: None,
                refs: Box::new(Refs {
                    branch: Some("agent/dispatch-x".to_string()),
                    ..Refs::default()
                }),
                workspace: workspace.clone(),
            };
            let attempt = |agent| AttemptId::new(run, agent).unwrap();
            let lines = [
                planted_line(run, 1, 2_000, opened(&older, Launch::Dispatch)),
                planted_line(
                    run,
                    2,
                    2_001,
                    Event::AttemptStarted {
                        work: older.clone(),
                        attempt: attempt(1),
                        pane: "pane-1".into(),
                        agent: 1,
                        role: None,
                    },
                ),
                planted_line(
                    run,
                    3,
                    2_002,
                    Event::AttemptEnded {
                        work: older.clone(),
                        attempt: attempt(1),
                        how: EndHow::Interrupted,
                        code: None,
                        cause: None,
                    },
                ),
                // The clock stepped back: a later line, an earlier time.
                planted_line(run, 4, 1_000, opened(&newer, Launch::IssueDispatch)),
                planted_line(
                    run,
                    5,
                    1_001,
                    Event::AttemptStarted {
                        work: newer.clone(),
                        attempt: attempt(2),
                        pane: "pane-2".into(),
                        agent: 2,
                        role: None,
                    },
                ),
                planted_line(
                    run,
                    6,
                    1_002,
                    Event::AttemptEnded {
                        work: newer.clone(),
                        attempt: attempt(2),
                        how: EndHow::Exited,
                        code: Some(0),
                        cause: None,
                    },
                ),
                planted_line(
                    run,
                    7,
                    1_003,
                    Event::Closed {
                        work: newer.clone(),
                        how: CloseHow::FireDone,
                    },
                ),
            ];
            let dir = state.join(crate::work_journal::DIR_NAME);
            std::fs::create_dir_all(&dir).unwrap();
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(dir.join("run-r0.jsonl"), lines.join("\n") + "\n").unwrap();
        });
        assert_eq!(wt_slot.lock().unwrap().as_deref(), Some(fx.wt.as_path()));
        let registry = &fx.registry;
        let listed_ids: Vec<WorkId> = listed(registry)
            .await
            .items
            .into_iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(
            listed_ids,
            vec![older.clone()],
            "fixture precondition: the planted older item is listed"
        );
        mark_naming(&fx.wt, &newer);

        fx.assert_older_is_refused(registry, &older).await;
    });
}

/// PR #2431 round 2, auditor A1 (late write): the newer run's launch
/// lands after the reclaim's first checks and before the removal. Played
/// here by rewriting the marker to name it, and writing its journal lines,
/// between the keep check and the last re-validation. The re-validation
/// reads the marker again, so the tree is kept and the item stays listed.
#[test]
fn reclaim_keeps_the_tree_when_a_newer_launch_marks_it_before_the_removal() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        mark_naming(&fx.wt, &older);
        let newer = journal(registry).mint_work_id().unwrap();
        {
            let wt = fx.wt.clone();
            let newer = newer.clone();
            let registry = Arc::clone(registry);
            super::seams::arm_before_revalidate(&fx.wt, move || {
                finished_launch(
                    &registry,
                    &newer,
                    7,
                    true,
                    journal(&registry).workspace(&wt),
                );
                mark_naming(&wt, &newer);
            });
        }

        let outcome = reclaim(registry, older.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptChanged, "{outcome:?}");
        assert!(
            fx.wt.join(".notes").is_file(),
            "the newer run's tree is kept"
        );
        assert!(closes(registry).iter().all(|(work, _)| *work != older));
    });
}

/// PR #2431 round 2: a marker written before it named the launch (an older
/// build's, or one whose launch could not be recorded) cannot show the tree
/// is still this item's, so the reclaim refuses, dry run or not, with its
/// own reason, and removes nothing. Dismissing the item still works.
#[test]
fn reclaim_refuses_a_worktree_whose_marker_names_no_launch() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        mark(&fx.wt);

        for dry_run in [true, false] {
            let outcome = reclaim(registry, older.to_string(), dry_run, None).await;
            assert_eq!(outcome.verdict, ReclaimVerdict::Refused, "{outcome:?}");
            let wire = serde_json::to_value(&outcome).unwrap();
            assert_eq!(
                wire["refusal"]["reason"], "owner-unconfirmed",
                "dry run {dry_run}: {outcome:?}"
            );
            assert_eq!(reason_of(&outcome), Some(RefusalReason::OwnerUnconfirmed));
            assert_eq!(
                outcome.message,
                crate::interrupted_work::REFUSED_OWNER_UNCONFIRMED
            );
            assert!(fx.wt.join(".notes").is_file(), "the tree is kept");
        }
        let dismissed = dismiss(registry, vec![older.to_string()], false, None).await;
        assert_eq!(dismissed.dismissed.len(), 1, "{dismissed:?}");
        assert!(fx.wt.is_dir());
    });
}

/// PR #2431 round 3 (reviewer R1): replace `wt`'s marker with a symlink to
/// a saved copy of a marker naming `launch`, kept outside the git metadata
/// directory, as a planted or restored marker would be.
#[cfg(unix)]
fn mark_through_symlink(wt: &Path, saved: &Path, launch: &WorkId) {
    mark_naming(wt, launch);
    let marker = crate::worktree_owner::marker_path(wt).expect("the marker's directory resolves");
    std::fs::rename(&marker, saved).expect("save the marker");
    std::os::unix::fs::symlink(saved, &marker).expect("link the marker to the saved copy");
}

/// PR #2431 round 3 (reviewer R1): a marker that is a symlink to a marker
/// naming the item's own launch is not the tree's own marker, so the
/// reclaim refuses it as owner unconfirmed, dry run or not, and the tree
/// with its committed work stays.
#[cfg(unix)]
#[test]
fn reclaim_keeps_a_tree_whose_marker_is_a_symlink() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        mark_through_symlink(&fx.wt, &fx.root_dir.join("saved-marker"), &older);

        for dry_run in [true, false] {
            let outcome = reclaim(registry, older.to_string(), dry_run, None).await;
            assert_eq!(outcome.verdict, ReclaimVerdict::Refused, "{outcome:?}");
            assert_eq!(
                reason_of(&outcome),
                Some(RefusalReason::OwnerUnconfirmed),
                "dry run {dry_run}: {outcome:?}"
            );
            assert!(fx.wt.join(".notes").is_file(), "the tree is kept");
        }
        assert!(closes(registry).iter().all(|(work, _)| *work != older));
    });
}

/// PR #2431 round 3 (reviewer R1), at the last re-read: the marker names
/// the item's launch when the reclaim first checks it and is then replaced
/// by a symlink to a copy of itself before the removal. The re-read does
/// not follow it, so the tree is kept.
#[cfg(unix)]
#[test]
fn reclaim_keeps_the_tree_when_its_marker_becomes_a_symlink_before_the_removal() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        mark_naming(&fx.wt, &older);
        {
            let wt = fx.wt.clone();
            let saved = fx.root_dir.join("saved-marker");
            let older = older.clone();
            super::seams::arm_before_revalidate(&fx.wt, move || {
                mark_through_symlink(&wt, &saved, &older);
            });
        }

        let outcome = reclaim(registry, older.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptChanged, "{outcome:?}");
        assert!(fx.wt.join(".notes").is_file(), "the tree is kept");
        assert!(closes(registry).iter().all(|(work, _)| *work != older));
    });
}

/// PR #2431 round 3 (reviewer R2): the last re-read, which runs while the
/// reclaim holds the repository's worktree lock, meets a FIFO where the
/// marker was, swapped in right before its open. It answers promptly
/// instead of waiting for a writer, and the tree is kept.
#[cfg(unix)]
#[test]
fn reclaim_keeps_the_tree_without_blocking_when_its_marker_becomes_a_fifo_before_the_open() {
    rt().block_on(async {
        let fx = NewerRunFixture::new();
        let registry = &fx.registry;
        let older = fx.older(registry);
        mark_naming(&fx.wt, &older);
        let marker = crate::worktree_owner::marker_path(&fx.wt).expect("marker path");
        {
            let marker = marker.clone();
            super::seams::arm_before_revalidate(&fx.wt, move || {
                let swapped = marker.clone();
                crate::worktree_owner::seams::arm_before_marker_open(&marker, move || {
                    std::fs::remove_file(&swapped).unwrap();
                    crate::worktree_owner::tests_support::mkfifo_at(&swapped);
                });
            });
        }

        let bounded = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reclaim(registry, older.to_string(), false, None),
        )
        .await;
        let Ok(outcome) = bounded else {
            // Give the stuck open its writer, so the runtime can shut down.
            crate::worktree_owner::tests_support::release_fifo_reader(&marker);
            panic!("the reclaim blocked on a FIFO at the marker path");
        };
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptChanged, "{outcome:?}");
        assert!(fx.wt.join(".notes").is_file(), "the tree is kept");
        assert!(closes(registry).iter().all(|(work, _)| *work != older));
    });
}

fn git_in(dir: &Path, root: &Path, args: &[&str]) {
    let out = crate::git_env::fixture_git(dir, root)
        .args(args)
        .output()
        .expect("git available");
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
}

fn reason_of(outcome: &ReclaimOutcome) -> Option<RefusalReason> {
    outcome.refusal.as_ref().map(|r| r.reason)
}

/// Auditor 1: a pane started through a symlink to the worktree — a spelling
/// neither the recorded nor the canonical path matches — keeps the reclaim
/// from removing the worktree, and so does one whose link was pointed at the
/// worktree only after it started (which only resolving at the reclaim
/// sees). Once the panes are gone the reclaim goes ahead.
#[test]
fn reclaim_refuses_while_a_pane_runs_in_the_worktree_through_a_symlink() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let s = |p: &Path| p.to_string_lossy().into_owned();

        // Started through a symlink to the worktree.
        let alias = root_dir.join("current-unit");
        std::os::unix::fs::symlink(&wt, &alias).unwrap();
        let through_alias = registry
            .spawn_agent(crate::agent_pty::SpawnOptions {
                command: Some("sleep 30"),
                cwd: Some(&s(&alias)),
                ..crate::agent_pty::SpawnOptions::default()
            })
            .expect("spawn a pane through the alias");
        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(
            reason_of(&outcome),
            Some(RefusalReason::StillInUse),
            "{outcome:?}"
        );
        assert!(wt.is_dir(), "nothing was removed under the live pane");
        registry.close_agent(&through_alias).unwrap();

        // Started through a link that pointed elsewhere then, and at the
        // worktree now.
        let elsewhere = root_dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let later = root_dir.join("later-link");
        std::os::unix::fs::symlink(&elsewhere, &later).unwrap();
        let repointed = registry
            .spawn_agent(crate::agent_pty::SpawnOptions {
                command: Some("sleep 30"),
                cwd: Some(&s(&later)),
                ..crate::agent_pty::SpawnOptions::default()
            })
            .expect("spawn a pane through the link");
        std::fs::remove_file(&later).unwrap();
        std::os::unix::fs::symlink(&wt, &later).unwrap();
        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(
            reason_of(&outcome),
            Some(RefusalReason::StillInUse),
            "{outcome:?}"
        );
        assert!(wt.is_dir());
        registry.close_agent(&repointed).unwrap();

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::Removed, "{outcome:?}");
        assert!(!wt.exists());
        registry.shutdown_all();
    });
}

/// Auditor 1: a start reserved in a directory whose identity cannot be
/// established (a `..` after a name that is not there) fails the reclaim
/// closed, and nothing is removed.
#[test]
fn reclaim_refuses_when_a_rooted_directory_cannot_be_resolved() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let unresolvable = root_dir.join("no-such-dir").join("..").join("wt");
        let reserved = registry
            .reserve_spawn_in_for_test(&unresolvable)
            .expect("reserve a start");

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(
            reason_of(&outcome),
            Some(RefusalReason::InUseUnknown),
            "{outcome:?}"
        );
        assert_eq!(
            outcome.message,
            crate::interrupted_work::REFUSED_IN_USE_UNKNOWN
        );
        assert!(wt.is_dir());
        registry.release_spawn_for_test(&reserved);
    });
}

/// Auditor 2: the worktree is replaced at its path (moved aside, and an
/// unmarked worktree on another branch added where it was) between the
/// reclaim's checks and its removal. The re-validation just before
/// `git worktree remove` sees it, and neither directory is removed or
/// recorded.
#[test]
fn reclaim_removes_nothing_when_the_worktree_is_replaced_before_removal() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let aside = root_dir.join("wt-aside");
        {
            let (repo, wt, aside, root_dir) =
                (repo.clone(), wt.clone(), aside.clone(), root_dir.clone());
            seams::arm_before_revalidate(&wt.clone(), move || {
                let (w, a) = (wt.to_string_lossy(), aside.to_string_lossy());
                git_in(&repo, &root_dir, &["worktree", "move", &w, &a]);
                git_in(
                    &repo,
                    &root_dir,
                    &["worktree", "add", "-q", "-b", "stranger", &w],
                );
                std::fs::write(wt.join("replacement-sentinel"), "keep me").unwrap();
            });
        }

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptChanged, "{outcome:?}");
        assert_eq!(
            outcome.message,
            crate::interrupted_work::RECLAIM_KEPT_CHANGED
        );
        assert!(!outcome.recorded);
        assert!(aside.is_dir(), "the original worktree is kept");
        assert!(
            wt.join("replacement-sentinel").is_file(),
            "the replacement is not removed"
        );
        assert!(closes(&registry).is_empty());
        assert_eq!(listed(&registry).await.items.len(), 1);
    });
}

/// Auditor 2: another branch is checked out in the worktree between the
/// reclaim's checks and its removal; nothing is removed.
#[test]
fn reclaim_removes_nothing_when_the_branch_is_switched_before_removal() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        {
            let (wt, root_dir) = (wt.clone(), root_dir.clone());
            seams::arm_before_revalidate(&wt.clone(), move || {
                git_in(&wt, &root_dir, &["switch", "-q", "-c", "switched"]);
            });
        }

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptChanged, "{outcome:?}");
        assert!(wt.is_dir(), "nothing was removed");
        assert!(closes(&registry).is_empty());
    });
}

/// Reviewer 2: a removal that succeeds after the reply's bound has passed
/// is still recorded as reclaimed, by the task that removed it, and the item
/// is no longer listed.
#[test]
fn a_removal_that_finishes_after_the_reply_bound_is_still_recorded() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, child) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        // The hook runs inside the reclaim's tokio task. A bare
        // `std::thread::sleep` there blocks that worker, which can keep the
        // 300 ms bound's timer from firing until the sleep ends when that
        // worker is the one driving timers, and the reply then reads Removed
        // (RWX release-lib on PR #2524, 2026-11-26, not reproduced in 25,584
        // runs since). `block_in_place` hands the worker's duties to another
        // thread for the sleep's length.
        seams::arm_after_removal(&wt, || {
            tokio::task::block_in_place(|| std::thread::sleep(Duration::from_millis(1500)))
        });

        let outcome = reclaim_within(
            &registry,
            root.to_string(),
            false,
            None,
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(outcome.verdict, ReclaimVerdict::TimedOut, "{outcome:?}");
        assert_eq!(outcome.message, crate::interrupted_work::RECLAIM_TIMED_OUT);

        // The task writes the root's close and the child's one after the
        // other, so wait for the whole set, not for the first line
        // (reviewer 4).
        let expected = vec![(root, CloseHow::Reclaimed), (child, CloseHow::Reclaimed)];
        let until = std::time::Instant::now() + Duration::from_secs(20);
        while closes(&registry) != expected && std::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(closes(&registry), expected);
        assert!(!wt.exists());
        assert!(listed(&registry).await.items.is_empty());
    });
}

/// Auditor 4: a dismissal naming more ids than one request may, an id
/// longer than one may be, or more bytes of ids in all, is refused whole
/// before anything is read or written.
#[test]
fn dismiss_refuses_an_oversized_selector_and_writes_nothing() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", None);
        let refused_whole = |outcome: &DismissOutcome| outcome.refusal.as_ref().map(|r| r.reason);

        let mut many: Vec<String> = (0..MAX_DISMISS_IDS).map(|n| format!("id-{n}")).collect();
        many.push(root.to_string());
        let long = vec![root.to_string(), "x".repeat(MAX_DISMISS_ID_BYTES + 1)];
        let bulky: Vec<String> = (0..(MAX_DISMISS_BYTES / MAX_DISMISS_ID_BYTES + 1))
            .map(|n| format!("{n:0>width$}", width = MAX_DISMISS_ID_BYTES))
            .collect();
        for ids in [many, long, bulky] {
            let outcome = dismiss(&registry, ids, false, None).await;
            assert_eq!(refused_whole(&outcome), Some(RefusalReason::TooLarge));
            assert_eq!(
                outcome.refusal.as_ref().unwrap().message,
                crate::interrupted_work::REFUSED_TOO_LARGE
            );
        }
        assert!(closes(&registry).is_empty());
        assert_eq!(listed(&registry).await.items.len(), 1);

        // At the limits it is answered, and the unmatched ids are refused by
        // name.
        let mut at_limit: Vec<String> = (1..MAX_DISMISS_IDS).map(|n| format!("id-{n}")).collect();
        at_limit.push(root.to_string());
        let outcome = dismiss(&registry, at_limit, false, None).await;
        assert_eq!(outcome.refusal, None);
        assert_eq!(outcome.dismissed.len(), 1);
        assert_eq!(
            outcome.refused.len() + outcome.refused_omitted,
            MAX_DISMISS_IDS - 1
        );
    });
}

/// Auditor 4: dismissing every item of a history with many roots dismisses
/// each one, past the ones its report names, and leaves nothing listed.
#[test]
fn dismiss_all_over_many_roots_dismisses_each_and_caps_its_report() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let roots = crate::interrupted_work::MAX_DISMISS_REPORTED + 44;
        for agent in 0..roots {
            interrupted_work(&registry, agent as u64 + 10, Launch::Pane, None, None, None);
        }
        assert_eq!(listed(&registry).await.items.len(), roots);

        let started = std::time::Instant::now();
        let outcome = dismiss(&registry, Vec::new(), true, None).await;
        assert_eq!(outcome.refusal, None);
        assert!(outcome.refused.is_empty(), "{:?}", outcome.refused);
        assert_eq!(
            outcome.dismissed.len(),
            crate::interrupted_work::MAX_DISMISS_REPORTED
        );
        assert_eq!(outcome.dismissed_omitted, 44);
        assert!(listed(&registry).await.items.is_empty());
        assert_eq!(closes(&registry).len(), roots);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "took {:?}",
            started.elapsed()
        );
    });
}

/// Auditor 7: the agent refusal reads the request's optional claim and
/// nothing else. No claim (absent, or `null` on the wire) is a person's
/// request and is acted on; any claim at all, one that would not attest
/// included, is refused; a claim of the wrong shape does not decode, so the
/// request is answered with an error and acts on nothing.
#[test]
fn the_agent_refusal_reads_the_optional_claim_as_its_contract() {
    use crate::daemon_protocol::AttachRequest;
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", None);
        let empty = CallerClaim {
            pane_id: String::new(),
            token: String::new(),
        };
        for claim in [claim(), empty] {
            let outcome = dismiss(&registry, vec![root.to_string()], false, Some(&claim)).await;
            assert_eq!(
                outcome.refusal.as_ref().map(|r| r.reason),
                Some(RefusalReason::AgentCaller)
            );
            assert_eq!(
                outcome.refusal.as_ref().unwrap().message,
                crate::interrupted_work::REFUSED_AGENT_CALLER
            );
            let reclaimed = reclaim(&registry, root.to_string(), true, Some(&claim)).await;
            assert_eq!(reason_of(&reclaimed), Some(RefusalReason::AgentCaller));
        }
        assert!(closes(&registry).is_empty());

        let decode = |text: &str| serde_json::from_str::<AttachRequest>(text);
        let missing = decode(&format!(
            r#"{{"op": "dismiss-interrupted", "ids": ["{root}"]}}"#
        ))
        .unwrap();
        let null = decode(&format!(
            r#"{{"op": "dismiss-interrupted", "ids": ["{root}"], "caller": null}}"#
        ))
        .unwrap();
        for request in [&missing, &null] {
            assert!(
                matches!(
                    request,
                    AttachRequest::DismissInterrupted { caller: None, .. }
                ),
                "{request:?}"
            );
        }
        assert!(
            decode(&format!(
                r#"{{"op": "dismiss-interrupted", "ids": ["{root}"], "caller": {{"pane_id": 5}}}}"#
            ))
            .is_err(),
            "a malformed claim does not decode"
        );
        assert!(decode(r#"{"op": "reclaim-interrupted", "id": "w", "caller": "agent"}"#).is_err());

        let AttachRequest::DismissInterrupted { ids, all, caller } = missing else {
            unreachable!()
        };
        let outcome = dismiss(&registry, ids, all, caller.as_ref()).await;
        assert_eq!(outcome.refusal, None, "no claim is a person's request");
        assert_eq!(outcome.dismissed.len(), 1);
    });
}

/// Reviewer 3: a show reads the body of any one of the item's reports by
/// its id, the seventeenth and later included, and refuses a report that is
/// not the item's; without one it reads the first [`MAX_SHOWN_REPORTS`].
#[test]
fn show_reads_any_one_of_the_items_reports_by_its_id() {
    use crate::interrupted_work::{ItemReport, fit_show};
    let mut item = InterruptedItem {
        id: WorkId::parse("w-abcdefgh-0000000000000001").unwrap(),
        kind: ItemKind::Unit,
        name: None,
        issue: None,
        repo: None,
        branch: None,
        worktree: None,
        launcher: None,
        last_entry: crate::interrupted_work::LastEntry {
            kind: "reported".into(),
            ts_ms: 1,
        },
        reports: Vec::new(),
        reports_omitted: 0,
        cut: false,
        progress: crate::interrupted_work::Progress::default(),
        git: None,
        agents: Vec::new(),
        agents_omitted: 0,
    };
    for n in 1..=20 {
        item.reports.push(ItemReport {
            report_id: format!("r-{n}"),
            task_id: None,
            unit_id: None,
            kept_until_ms: None,
        });
    }
    let body = |report_id: &str| ShownReport {
        report_id: report_id.to_string(),
        body: Some(format!("BODY OF {report_id}")),
        cut: false,
        error: None,
    };

    let first = shown_bodies(&item, None).unwrap();
    assert_eq!(first.len(), MAX_SHOWN_REPORTS);
    assert_eq!(first[0], "r-1");

    let wanted = shown_bodies(&item, Some("r-18")).unwrap();
    assert_eq!(wanted, vec!["r-18".to_string()]);
    let shown = fit_show(item.clone(), &wanted, false, MAX_REPLY_BYTES, body);
    assert_eq!(shown.reports.len(), 1);
    assert_eq!(shown.reports[0].report_id, "r-18");
    assert_eq!(shown.reports[0].body.as_deref(), Some("BODY OF r-18"));
    assert_eq!(shown.item.reports.len(), 20, "every entry is listed");

    assert_eq!(
        shown_bodies(&item, Some("r-99")),
        Err(crate::interrupted_work::REPORT_NOT_OF_ITEM.to_string())
    );
}

/// Reviewer 3: through the daemon, a report that is not the item's is an
/// error answer, and naming none answers the item.
#[test]
fn show_refuses_a_report_that_is_not_the_items() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", None);
        assert_eq!(
            show(&registry, root.to_string(), Some("r-nope".into())).await,
            Err(crate::interrupted_work::REPORT_NOT_OF_ITEM.to_string())
        );
        let shown = show(&registry, root.to_string(), None).await.unwrap();
        assert_eq!(shown.item.id, root);
        assert_eq!(shown.item.progress.attempts, 2);
    });
}

/// A dispatched unit, interrupted, whose `opened` names it `unit_id` and
/// records `branch`.
fn interrupted_unit_named(
    registry: &AgentPtyRegistry,
    agent: u64,
    unit_id: &str,
    branch: &str,
    workspace: Option<&Path>,
) -> WorkId {
    let journal = journal(registry);
    let work = journal.mint_work_id().unwrap();
    let attempt = AttemptId::new(RUN, agent).unwrap();
    record(
        registry,
        Event::Opened {
            work: work.clone(),
            launch: Launch::Dispatch,
            parent: None,
            refs: Box::new(Refs {
                branch: Some(branch.to_string()),
                unit_id: Some(unit_id.to_string()),
                ..Refs::default()
            }),
            workspace: workspace.and_then(|path| journal.workspace(path)),
        },
    );
    record(
        registry,
        Event::AttemptStarted {
            work: work.clone(),
            attempt: attempt.clone(),
            pane: format!("pane-{agent}"),
            agent,
            role: None,
        },
    );
    record(
        registry,
        Event::AttemptEnded {
            work: work.clone(),
            attempt,
            how: EndHow::Interrupted,
            code: None,
            cause: None,
        },
    );
    work
}

/// Auditor N1: the person confirmed item A by its full id, and A was
/// dismissed by another client before their request arrived. Item B, whose
/// name is A's id, is not dismissed or reclaimed in A's place: both
/// requests are refused as not listed, and B's journal and clean, owned
/// worktree are left as they were.
#[test]
fn a_stale_full_id_is_refused_rather_than_retargeted_to_another_item() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let a = interrupted_work(&registry, 1, Launch::Pane, None, None, None);
        let gone = dismiss(&registry, vec![a.to_string()], false, None).await;
        assert_eq!(gone.dismissed.len(), 1, "{gone:?}");
        let b = interrupted_unit_named(&registry, 2, a.as_str(), "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &b);
        let now = listed(&registry).await;
        assert_eq!(now.items.len(), 1);
        assert_eq!(now.items[0].id, b);
        assert_eq!(now.items[0].name.as_deref(), Some(a.as_str()));
        let closes_before = closes(&registry);

        let dismissed = dismiss(&registry, vec![a.to_string()], false, None).await;
        assert!(dismissed.dismissed.is_empty(), "{dismissed:?}");
        assert_eq!(dismissed.refused.len(), 1, "{dismissed:?}");
        assert_eq!(
            dismissed.refused[0].refusal.reason,
            RefusalReason::NotListed
        );

        let reclaimed = reclaim(&registry, a.to_string(), false, None).await;
        assert_eq!(
            reason_of(&reclaimed),
            Some(RefusalReason::NotListed),
            "{reclaimed:?}"
        );
        assert!(wt.is_dir(), "B's worktree is not removed");
        assert_eq!(closes(&registry), closes_before, "nothing more was closed");
        assert_eq!(listed(&registry).await.items.len(), 1, "B is still listed");

        // B is still reached by its own id.
        let dry = reclaim(&registry, b.to_string(), true, None).await;
        assert_eq!(dry.verdict, ReclaimVerdict::WouldRemove, "{dry:?}");
    });
}

/// Auditor N2 / reviewer 2: items whose launch records carry names as long
/// as the record reader accepts are dismissed, and the dismissal's answer
/// still fits the reply budget (and so the attach frame): each name is cut
/// to the field bound, and every item dismissed is reported or counted.
#[test]
fn a_dismissal_of_items_with_oversized_names_fits_its_reply() {
    use crate::interrupted_work::{MAX_FIELD_BYTES, json_len};
    use crate::launch_records::{LaunchRecord, SCHEMA_VERSION};
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let items = 48usize;
        let name_bytes = 200 * 1024;
        for n in 0..items {
            let agent = n as u64 + 10;
            let work = interrupted_work(&registry, agent, Launch::Pane, None, None, None);
            crate::launch_records::write(
                state.path(),
                &LaunchRecord {
                    v: SCHEMA_VERSION,
                    work,
                    attempt: AttemptId::new(RUN, agent).unwrap(),
                    command: None,
                    shell: None,
                    agent_type: None,
                    cwd: None,
                    cwd_moved_from: None,
                    name: Some(format!("{n:04}{}", "n".repeat(name_bytes))),
                    role: None,
                    orchestration: None,
                    orchestration_id: None,
                    ts_ms: 1,
                    pid: None,
                    pid_start: None,
                    launch_nonce: None,
                },
            )
            .expect("write a launch record");
        }
        let before = listed(&registry).await;
        assert_eq!(before.items.len() + before.omitted, items);

        let outcome = dismiss(&registry, Vec::new(), true, None).await;
        assert_eq!(outcome.refusal, None);
        let size = json_len(&outcome);
        assert!(
            size <= MAX_REPLY_BYTES,
            "the dismissal answer is {size} bytes"
        );
        assert!(
            outcome
                .dismissed
                .iter()
                .all(|d| d.name.as_ref().is_none_or(|n| n.len() <= MAX_FIELD_BYTES)),
            "every name is cut to the field bound"
        );
        assert_eq!(outcome.dismissed.len() + outcome.dismissed_omitted, items);
        assert_eq!(closes(&registry).len(), items);
        assert!(listed(&registry).await.items.is_empty());
    });
}

/// Auditor A2: right after the re-validation's first identity check, the
/// worktree is moved aside and an exact copy of it (the same `.git`
/// pointer, so the same git metadata directory, owner marker and branch) is
/// put at its path. Every later Git check passes for the copy; the identity
/// check repeated just before `git worktree remove` sees another directory,
/// and nothing is removed or recorded.
#[test]
fn reclaim_removes_nothing_when_the_worktree_is_swapped_after_the_first_identity_check() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let aside = root_dir.join("wt-aside");
        {
            let (wt, aside) = (wt.clone(), aside.clone());
            seams::arm_after_first_identity(&wt.clone(), move || {
                std::fs::rename(&wt, &aside).unwrap();
                let out = std::process::Command::new("cp")
                    .arg("-a")
                    .arg(&aside)
                    .arg(&wt)
                    .output()
                    .expect("cp available");
                assert!(out.status.success(), "{out:?}");
            });
        }

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptChanged, "{outcome:?}");
        assert!(!outcome.recorded);
        assert!(aside.is_dir(), "the original worktree is kept");
        assert!(wt.is_dir(), "the copy at its path is not removed");
        assert!(closes(&registry).is_empty());
        assert_eq!(listed(&registry).await.items.len(), 1);
    });
}

/// Auditor A2: between the keep check and the re-validation, another
/// worktree of the repository is added inside the item's worktree, under a
/// directory Git ignores (so the outer tree still reads clean), with an
/// unsaved file in it. The nested-worktree check is made again after the
/// re-validation, so the tree is kept and the nested worktree named; neither
/// it nor its file is removed.
#[test]
fn reclaim_keeps_a_worktree_nested_inside_it_after_the_keep_check() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        let exclude = repo.join(".git").join("info").join("exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        let mut patterns = std::fs::read_to_string(&exclude).unwrap_or_default();
        patterns.push_str("\n.worktrees/\n");
        std::fs::write(&exclude, patterns).unwrap();
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let nested = wt.join(".worktrees").join("nested");
        {
            let (repo, nested, root_dir) = (repo.clone(), nested.clone(), root_dir.clone());
            seams::arm_before_revalidate(&wt.clone(), move || {
                git_in(
                    &repo,
                    &root_dir,
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        "nested",
                        &nested.to_string_lossy(),
                    ],
                );
                std::fs::write(nested.join("unsaved.txt"), "keep me").unwrap();
            });
        }

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(
            outcome.verdict,
            ReclaimVerdict::KeptHoldsWorktrees,
            "{outcome:?}"
        );
        assert_eq!(outcome.nested_worktrees.len(), 1, "{outcome:?}");
        assert!(
            nested.join("unsaved.txt").is_file(),
            "the nested file is kept"
        );
        assert!(wt.is_dir());
        assert!(closes(&registry).is_empty());
    });
}

/// Auditor A2: a reclaim takes the repository's worktree lock (the one the
/// deck's worktree creation and removal take) across its keep check,
/// re-validation and removal, and fails closed when it cannot: with the lock
/// held elsewhere, nothing is removed or recorded, and the answer says so.
/// Once the lock is free the reclaim goes ahead.
#[test]
fn reclaim_removes_nothing_when_the_repositorys_worktree_lock_cannot_be_taken() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let lock_path = crate::issue_dispatch_run::worktree_lock_path(&repo)
            .await
            .expect("a lock path");
        let held = crate::platform::lock::acquire_spawn_lock(&lock_path)
            .await
            .expect("take the lock");

        let outcome = reclaim_with(
            &registry,
            root.to_string(),
            false,
            None,
            RECLAIM_TIMEOUT,
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(outcome.verdict, ReclaimVerdict::KeptBusy, "{outcome:?}");
        assert_eq!(outcome.message, crate::interrupted_work::RECLAIM_KEPT_BUSY);
        assert!(wt.is_dir(), "nothing was removed");
        assert!(closes(&registry).is_empty());

        drop(held);
        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(outcome.verdict, ReclaimVerdict::Removed, "{outcome:?}");
        assert!(!wt.exists());
    });
}

/// Reviewer 1: an item keeps more reports than a list's or a show's report
/// entries carry. The list carries the first [`MAX_LISTED_REPORTS`] and
/// says where the next page starts; each show page asked for with
/// `reports_from` carries the next entries and their bodies and says where
/// the page after it starts, until the last. Every report is found that
/// way, and the last one, beyond the list's cap, is then read by its id.
///
/// [`MAX_LISTED_REPORTS`]: crate::interrupted_work::MAX_LISTED_REPORTS
#[test]
fn show_pages_reach_every_kept_report_beyond_the_metadata_cap() {
    use crate::interrupted_work::{
        ItemReport, MAX_LISTED_REPORTS, fit_list, fit_show, fit_show_page,
    };
    let total = 40;
    let mut item = InterruptedItem {
        id: WorkId::parse("w-abcdefgh-0000000000000001").unwrap(),
        kind: ItemKind::Unit,
        name: None,
        issue: None,
        repo: None,
        branch: None,
        worktree: None,
        launcher: None,
        last_entry: crate::interrupted_work::LastEntry {
            kind: "reported".into(),
            ts_ms: 1,
        },
        reports: Vec::new(),
        reports_omitted: 0,
        cut: false,
        progress: crate::interrupted_work::Progress::default(),
        git: None,
        agents: Vec::new(),
        agents_omitted: 0,
    };
    for n in 1..=total {
        item.reports.push(ItemReport {
            report_id: format!("r-{n}"),
            task_id: None,
            unit_id: None,
            kept_until_ms: None,
        });
    }
    let body = |report_id: &str| ShownReport {
        report_id: report_id.to_string(),
        body: Some(format!("BODY OF {report_id}")),
        cut: false,
        error: None,
    };

    let mut list = InterruptedList {
        items: vec![item.clone()],
        incomplete: false,
        omitted: 0,
    };
    fit_list(&mut list, MAX_REPLY_BYTES, 0);
    let listed = &list.items[0];
    assert_eq!(listed.reports.len(), MAX_LISTED_REPORTS);
    assert_eq!(listed.reports_omitted, total - MAX_LISTED_REPORTS);
    let mut found: Vec<String> = listed.reports.iter().map(|r| r.report_id.clone()).collect();

    let mut next = Some(listed.reports.len());
    let mut pages = 0;
    while let Some(from) = next {
        pages += 1;
        assert!(pages <= total, "paging does not end");
        let bodies = shown_bodies_page(&item, None, Some(from)).unwrap();
        let page = fit_show_page(
            item.clone(),
            Some(from),
            &bodies,
            false,
            MAX_REPLY_BYTES,
            body,
        );
        assert_eq!(page.reports_from, from, "the page starts where asked");
        assert!(!page.item.reports.is_empty());
        assert!(page.item.reports.len() <= MAX_SHOWN_REPORTS);
        assert_eq!(
            page.reports
                .iter()
                .map(|r| &r.report_id)
                .collect::<Vec<_>>(),
            page.item
                .reports
                .iter()
                .map(|r| &r.report_id)
                .collect::<Vec<_>>(),
            "a page's bodies are its own entries'"
        );
        assert_eq!(
            page.item.reports.len() + page.item.reports_omitted,
            total,
            "the rest of the item's reports are counted"
        );
        found.extend(page.item.reports.iter().map(|r| r.report_id.clone()));
        next = page.next_reports_from;
        if let Some(after) = next {
            assert_eq!(after, from + page.item.reports.len());
        }
    }
    let all: Vec<String> = (1..=total).map(|n| format!("r-{n}")).collect();
    assert_eq!(found, all, "every report is found, once, in order");

    let last = shown_bodies(&item, Some("r-40")).unwrap();
    let shown = fit_show(item.clone(), &last, false, MAX_REPLY_BYTES, body);
    assert_eq!(shown.reports[0].body.as_deref(), Some("BODY OF r-40"));
}

/// The repository's worktree lock held by another process stand-in: an open
/// file description of its own with an exclusive `flock`, taken on the test's
/// thread rather than in the runtime's blocking pool, and released when
/// dropped.
fn hold_lock_elsewhere(lock_path: &Path) -> std::fs::File {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .expect("open the lock file");
    file.lock().expect("take the lock");
    file
}

/// Auditor N5 / reviewer P2: a reclaim that gives up waiting for the
/// repository's worktree lock leaves nothing waiting for it. On a runtime
/// with two blocking threads, with the lock held elsewhere throughout, four
/// reclaims in a row each answer `kept-busy` within their bound, and a
/// blocking job queued after them still runs: no abandoned lock wait holds a
/// blocking thread.
#[test]
fn busy_reclaims_leave_no_lock_waiter_behind_in_the_blocking_pool() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .unwrap();
    let tmp = crate::test_temp::tempdir().unwrap();
    let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
    let (repo, wt) = repo_with_worktree(&root_dir);
    let lock_path = rt
        .block_on(crate::issue_dispatch_run::worktree_lock_path(&repo))
        .expect("a lock path");
    let held = hold_lock_elsewhere(&lock_path);
    let answers = rt.block_on(async {
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let mut answers = Vec::new();
        for _ in 0..4 {
            let outcome = tokio::time::timeout(
                Duration::from_secs(20),
                reclaim_with(
                    &registry,
                    root.to_string(),
                    false,
                    None,
                    RECLAIM_TIMEOUT,
                    Duration::from_millis(100),
                ),
            )
            .await;
            answers.push(outcome.map(|o| o.verdict).ok());
        }
        let heartbeat =
            tokio::time::timeout(Duration::from_secs(10), tokio::task::spawn_blocking(|| 7))
                .await
                .ok()
                .and_then(Result::ok);
        (answers, heartbeat)
    });
    // Released before asserting, so a failure cannot leave the runtime
    // waiting on a blocked `flock` at shutdown.
    drop(held);
    rt.shutdown_timeout(Duration::from_secs(10));
    let (answers, heartbeat) = answers;
    assert_eq!(answers, vec![Some(ReclaimVerdict::KeptBusy); 4]);
    assert_eq!(heartbeat, Some(7), "a blocking job queued after them runs");
    assert!(wt.is_dir(), "nothing was removed");
}

/// Reviewer P3: a reclaim that cannot take the lock because the lock file
/// cannot be opened (a directory sits at its path) removes nothing and says
/// the lock could not be taken, not that another change was running; the
/// lock held elsewhere is still `kept-busy` (the test above, and
/// `reclaim_removes_nothing_when_the_repositorys_worktree_lock_cannot_be_taken`).
#[test]
fn a_lock_that_cannot_be_opened_is_not_reported_as_another_change() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let lock_path = crate::issue_dispatch_run::worktree_lock_path(&repo)
            .await
            .expect("a lock path");
        std::fs::create_dir(&lock_path).unwrap();

        let outcome = reclaim_with(
            &registry,
            root.to_string(),
            false,
            None,
            RECLAIM_TIMEOUT,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            outcome.verdict,
            ReclaimVerdict::KeptCouldNotLock,
            "{outcome:?}"
        );
        assert_eq!(
            outcome.message,
            crate::interrupted_work::RECLAIM_KEPT_COULD_NOT_LOCK
        );
        assert!(!outcome.message.contains("another"), "{}", outcome.message);
        assert!(wt.is_dir(), "nothing was removed");
        assert!(closes(&registry).is_empty());
    });
}

/// Auditor wire check: a page asked for past the item's reports, up to
/// `usize::MAX`, answers an empty page clamped to where the reports end,
/// with no next page and the reports still counted, rather than panicking
/// on a slice or an addition; and through the daemon's handler too.
#[test]
fn a_report_page_past_the_end_up_to_usize_max_is_clamped() {
    use crate::interrupted_work::{ItemReport, LastEntry, Progress, fit_show_page};
    let total = 40;
    let item = InterruptedItem {
        id: WorkId::parse("w-abcdefgh-0000000000000001").unwrap(),
        kind: ItemKind::Unit,
        name: None,
        issue: None,
        repo: None,
        branch: None,
        worktree: None,
        launcher: None,
        last_entry: LastEntry {
            kind: "reported".into(),
            ts_ms: 1,
        },
        reports: (1..=total)
            .map(|n| ItemReport {
                report_id: format!("r-{n}"),
                task_id: None,
                unit_id: None,
                kept_until_ms: None,
            })
            .collect(),
        reports_omitted: 0,
        cut: false,
        progress: Progress::default(),
        git: None,
        agents: Vec::new(),
        agents_omitted: 0,
    };
    let body = |report_id: &str| ShownReport {
        report_id: report_id.to_string(),
        body: Some(format!("BODY OF {report_id}")),
        cut: false,
        error: None,
    };
    for from in [total, total + 1, usize::MAX / 2, usize::MAX - 1, usize::MAX] {
        let bodies = shown_bodies_page(&item, None, Some(from)).unwrap();
        assert!(bodies.is_empty(), "{from}");
        let page = fit_show_page(
            item.clone(),
            Some(from),
            &bodies,
            false,
            MAX_REPLY_BYTES,
            body,
        );
        assert_eq!(page.reports_from, total, "{from}");
        assert!(page.item.reports.is_empty(), "{from}");
        assert!(page.reports.is_empty(), "{from}");
        assert_eq!(page.item.reports_omitted, total, "{from}");
        assert_eq!(page.next_reports_from, None, "{from}");
    }

    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", None);
        let shown = show_page(&registry, root.to_string(), None, Some(usize::MAX))
            .await
            .expect("an answer");
        assert_eq!(shown.item.id, root);
        assert_eq!(shown.reports_from, 0);
        assert_eq!(shown.next_reports_from, None);
    });
}

// ---------------------------------------------------------------------------
// Final review: a nested worktree of another repository (auditor 1), Dismiss
// all in batches against the real handler (reviewer 1 / auditor 2), and the
// keep check's children bounded (auditor 3).
// ---------------------------------------------------------------------------

/// A second, independent repository at `<root>/repo-b` with one commit, and a
/// linked worktree of it at `at` on branch `b-child`, holding an unsaved file
/// `unsaved.txt`.
fn foreign_worktree_at(root: &Path, at: &Path) {
    let other = root.join("repo-b");
    std::fs::create_dir_all(&other).unwrap();
    git_in(&other, root, &["init", "-q", "."]);
    crate::worktree_owner::pin_fixture_eol(&other, root);
    std::fs::write(other.join("b.txt"), "b").unwrap();
    git_in(&other, root, &["add", "."]);
    git_in(&other, root, &["commit", "-qm", "b"]);
    git_in(
        &other,
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b-child",
            &at.to_string_lossy(),
        ],
    );
    std::fs::write(at.join("unsaved.txt"), "keep me").unwrap();
}

/// Ignore `.worktrees/` in `repo` and every worktree of it.
fn ignore_worktrees_dir(repo: &Path) {
    let exclude = repo.join(".git").join("info").join("exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    let mut patterns = std::fs::read_to_string(&exclude).unwrap_or_default();
    patterns.push_str("\n.worktrees/\n");
    std::fs::write(&exclude, patterns).unwrap();
}

/// Auditor 1: the item's worktree, a linked worktree of repository A, holds
/// under its git-ignored `.worktrees/` a linked worktree of an independent
/// repository B with an unsaved file. A's `git status` is clean and A's
/// `git worktree list` does not know B's worktree, so only a look at the
/// directory finds it. The dry run and the real reclaim both keep the tree
/// and name the nested one; both directories and the file survive, and no
/// reclaimed close is written.
#[test]
fn reclaim_keeps_a_worktree_holding_another_repositorys_worktree() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        ignore_worktrees_dir(&repo);
        let child = wt.join(".worktrees").join("b-child");
        foreign_worktree_at(&root_dir, &child);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);

        for dry_run in [true, false] {
            let outcome = reclaim(&registry, root.to_string(), dry_run, None).await;
            assert_eq!(
                outcome.verdict,
                ReclaimVerdict::KeptHoldsWorktrees,
                "dry_run={dry_run}: {outcome:?}"
            );
            assert!(
                outcome
                    .nested_worktrees
                    .iter()
                    .any(|nested| nested.ends_with(".worktrees/b-child")),
                "dry_run={dry_run}: the child is named: {outcome:?}"
            );
            assert!(wt.is_dir() && child.is_dir(), "dry_run={dry_run}");
            assert_eq!(
                std::fs::read_to_string(child.join("unsaved.txt"))
                    .ok()
                    .as_deref(),
                Some("keep me"),
                "dry_run={dry_run}"
            );
            assert!(closes(&registry).is_empty(), "dry_run={dry_run}");
        }
    });
}

/// Auditor 1, the final removal guard: another repository's worktree appears
/// under the item's worktree after the keep check, before the re-validation.
/// The guard's own look finds it, so nothing is removed.
#[test]
fn reclaim_keeps_a_worktree_another_repository_nests_in_after_the_keep_check() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&root_dir);
        ignore_worktrees_dir(&repo);
        let child = wt.join(".worktrees").join("b-child");
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        {
            let (root_dir, child) = (root_dir.clone(), child.clone());
            seams::arm_before_revalidate(&wt.clone(), move || {
                foreign_worktree_at(&root_dir, &child);
            });
        }

        let outcome = reclaim(&registry, root.to_string(), false, None).await;
        assert_eq!(
            outcome.verdict,
            ReclaimVerdict::KeptHoldsWorktrees,
            "{outcome:?}"
        );
        assert!(child.join("unsaved.txt").is_file());
        assert!(wt.is_dir());
        assert!(closes(&registry).is_empty());
    });
}

/// Reviewer 1 / auditor 2, against the real handler and journal: 257
/// interrupted items, one more than one dismissal may name. Split by the
/// shared [`crate::interrupted_work::dismiss_batches`], as the TUI, the
/// desktop and the CLI send them, every batch is admitted and dismisses its
/// items; an item that became interrupted after the ids were captured stays
/// listed.
#[test]
fn dismissing_more_items_than_one_request_admits_in_batches_dismisses_exactly_them() {
    rt().block_on(async {
        let state = crate::test_temp::tempdir().unwrap();
        let registry = registry_with_journal(state.path(), JournalOptions::default());
        for agent in 0..257u64 {
            interrupted_work(&registry, agent + 10, Launch::Pane, None, None, None);
        }
        let captured: Vec<String> = listed(&registry)
            .await
            .items
            .iter()
            .map(|item| item.id.to_string())
            .collect();
        assert_eq!(captured.len(), 257);
        let late = interrupted_work(&registry, 9_999, Launch::Pane, None, None, None);

        let batches = crate::interrupted_work::dismiss_batches(&captured);
        assert!(
            batches.iter().all(|batch| selector_fits(batch)),
            "every batch is admitted: {:?}",
            batches.iter().map(Vec::len).collect::<Vec<_>>()
        );
        let mut dismissed = 0;
        for batch in batches {
            let outcome = dismiss(&registry, batch, false, None).await;
            assert_eq!(outcome.refusal, None, "{outcome:?}");
            dismissed += outcome.dismissed_count();
        }
        assert_eq!(dismissed, 257);
        let left = listed(&registry).await.items;
        assert_eq!(
            left.iter()
                .map(|item| item.id.to_string())
                .collect::<Vec<_>>(),
            vec![late.to_string()]
        );
    });
}

/// A stand-in for the keep check's `git`: `status` and `worktree` each either
/// flood stdout forever (`flood`), stall with a background descendant,
/// writing the pids to `<dir>/<what>-leader.pid` and `<dir>/<what>-child.pid`
/// (`stall`), or run the real `git` (`real`); `status` may also answer clean
/// (`clean`).
fn keep_check_stand_in(dir: &Path, status: &str, worktree: &str) -> PathBuf {
    let script = dir.join("keep-check-git");
    let act = |what: &str, how: &str| match how {
        "flood" => "exec yes 'M a-flooded-line-of-git-output'".to_string(),
        "stall" => format!(
            "echo $$ > '{dir}/{what}-leader.pid'; sleep 30 & echo $! > '{dir}/{what}-child.pid'; wait",
            dir = dir.display()
        ),
        "clean" => "exit 0".to_string(),
        _ => format!("exec {} \"$@\"", crate::git_env::GIT),
    };
    crate::test_isolation::write_script(
        &script,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *status*) {} ;;\n  *worktree*) {} ;;\n  *) exec {} \"$@\" ;;\nesac\n",
            act("status", status),
            act("worktree", worktree),
            crate::git_env::GIT
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn pid_in(file: &Path) -> Option<libc::pid_t> {
    std::fs::read_to_string(file).ok()?.trim().parse().ok()
}

/// Whether `pid` runs no more (gone, or a zombie on Linux).
fn gone(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let missing = unsafe { libc::kill(pid, 0) } != 0;
    missing
        || std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z'))
}

fn soon_gone(pid: libc::pid_t) -> bool {
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < until {
        if gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    gone(pid)
}

/// Auditor 3: the keep check's `git status` and `git worktree list` run
/// through the bounded probe runner. Output past its cap kills the child and
/// keeps the tree: a flooded status reads as uncommitted changes, a flooded
/// listing as a check that could not be made. The dry run and the real
/// reclaim both answer, keep the tree and write no close.
#[test]
fn reclaim_keeps_the_tree_when_the_keep_checks_output_floods() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        for (status, worktree, kept) in [
            ("flood", "real", ReclaimVerdict::KeptDirty),
            ("clean", "flood", ReclaimVerdict::KeptCouldNotCheck),
        ] {
            let script = keep_check_stand_in(&root_dir, status, worktree);
            seams::arm_keep_check(&wt, script, Duration::from_secs(20));
            for dry_run in [true, false] {
                let outcome = tokio::time::timeout(
                    Duration::from_secs(25),
                    reclaim(&registry, root.to_string(), dry_run, None),
                )
                .await
                .expect("the reclaim answers");
                assert_eq!(
                    outcome.verdict, kept,
                    "status={status} worktree={worktree} dry_run={dry_run}: {outcome:?}"
                );
                assert!(wt.is_dir());
                assert!(closes(&registry).is_empty());
            }
            seams::disarm_keep_check(&wt);
        }
    });
}

/// Auditor 3: a keep check whose `git status` never answers is killed at the
/// check's deadline, with its descendant, and reaped before the reclaim
/// answers; the tree is kept as a check that could not be made. Asked again
/// after it answered, it does the same, and no child is left behind.
#[test]
fn reclaim_kills_and_reaps_a_stalled_keep_check_and_keeps_the_tree() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root_dir = std::fs::canonicalize(tmp.path()).unwrap();
        let (_repo, wt) = repo_with_worktree(&root_dir);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
        mark_naming(&wt, &root);
        let script = keep_check_stand_in(&root_dir, "stall", "real");
        seams::arm_keep_check(&wt, script, Duration::from_secs(1));
        for dry_run in [true, true, false] {
            let _ = std::fs::remove_file(root_dir.join("status-leader.pid"));
            let started = std::time::Instant::now();
            let outcome = tokio::time::timeout(
                Duration::from_secs(25),
                reclaim(&registry, root.to_string(), dry_run, None),
            )
            .await
            .expect("the reclaim answers");
            assert_eq!(
                outcome.verdict,
                ReclaimVerdict::KeptCouldNotCheck,
                "dry_run={dry_run}: {outcome:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "answered by the check's deadline: {:?}",
                started.elapsed()
            );
            let leader = pid_in(&root_dir.join("status-leader.pid")).expect("the child ran");
            let child = pid_in(&root_dir.join("status-child.pid")).expect("its descendant ran");
            assert!(
                gone(leader),
                "the stalled child was reaped before the answer"
            );
            assert!(soon_gone(child), "its descendant was killed with it");
            assert!(wt.is_dir());
            assert!(closes(&registry).is_empty());
        }
        seams::disarm_keep_check(&wt);
    });
}

/// PR #2431 review (Qodo): the look at whether an item's worktree exists is
/// bounded by the item's Git deadline. A lookup that stalls, as one on a hung
/// mount would, answers the facts as unknown once the deadline passes rather
/// than holding the list.
#[test]
fn git_facts_answer_unknown_when_the_worktree_lookup_stalls() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let stalled = std::fs::canonicalize(tmp.path())
            .unwrap()
            .join("on-a-hung-mount");
        seams::arm_metadata_stall(&stalled, Duration::from_secs(60));
        let deadline = Deadline::after(Duration::from_millis(200));
        let facts = tokio::time::timeout(
            Duration::from_secs(10),
            git_facts(
                ItemKind::Unit,
                Some(stalled.to_string_lossy().into_owned()),
                Some("agent/dispatch-x".into()),
                OsStr::new("gh"),
                deadline,
                deadline,
            ),
        )
        .await
        .expect("the facts answer by their deadline, not after the stalled lookup");
        assert_eq!(facts, GitFacts::default(), "unknown: {facts:?}");
        assert_eq!(facts.worktree_exists, None);
    });
}

/// PRD #1790 stage 3 (§4 item 10): a restore's or start-over's dry run reads
/// the item's Git facts with the list's bounded probes, so an open pull
/// request on the item's branch reaches the client's question.
#[cfg(unix)]
#[test]
fn a_dry_runs_git_facts_show_an_open_pull_request() {
    use std::os::unix::fs::PermissionsExt as _;
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_env::fixture_git(&repo, &root)
                .args(args)
                .output()
                .expect("git available");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "agent/dispatch-x", "."]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "a"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/someone/something.git",
        ]);
        let gh = root.join("gh");
        let answer =
            r#"[{"state":"OPEN","headRefName":"agent/dispatch-x","isCrossRepository":false}]"#;
        crate::test_isolation::write_script(&gh, format!("#!/bin/sh\nprintf '%s\\n' '{answer}'\n"))
            .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let item = InterruptedItem {
            id: WorkId::parse("w-abcdefgh-0000000000000001").unwrap(),
            kind: ItemKind::Unit,
            name: None,
            issue: None,
            repo: None,
            branch: Some("agent/dispatch-x".into()),
            worktree: Some(repo.to_string_lossy().into_owned()),
            launcher: None,
            last_entry: crate::interrupted_work::LastEntry {
                kind: "ended".into(),
                ts_ms: 1,
            },
            reports: Vec::new(),
            reports_omitted: 0,
            cut: false,
            progress: crate::interrupted_work::Progress::default(),
            git: None,
            agents: Vec::new(),
            agents_omitted: 0,
        };
        let facts = item_git_facts_with(&item, gh.as_os_str()).await;
        assert_eq!(facts.worktree_exists, Some(true));
        assert_eq!(facts.pr, crate::interrupted_work::PrFact::Open, "{facts:?}");
        let gone = InterruptedItem {
            worktree: Some(root.join("gone").to_string_lossy().into_owned()),
            ..item
        };
        let facts = item_git_facts_with(&gone, gh.as_os_str()).await;
        assert_eq!(facts.worktree_exists, Some(false));
        assert_eq!(facts.pr, crate::interrupted_work::PrFact::Unknown);
    });
}
