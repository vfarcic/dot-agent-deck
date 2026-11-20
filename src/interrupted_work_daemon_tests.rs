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

fn mark(wt: &Path) {
    crate::worktree_owner::write_marker(
        wt,
        "agent/dispatch-x",
        &crate::worktree_owner::Creator::dispatch("x"),
    )
    .expect("marker written");
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
        mark(&wt);
        let (other_branch, _) = unit(&registry, "agent/dispatch-y", Some(&wt));
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
        mark(&wt);
        std::fs::write(wt.join("scratch.txt"), "work").unwrap();
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));

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
        mark(&wt);
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
        mark(&wt);
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
        mark(&wt);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
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
        mark(&wt);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
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
        mark(&wt);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
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
        mark(&wt);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, _) = unit(&registry, "agent/dispatch-x", Some(&wt));
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
        mark(&wt);
        let registry = registry_with_journal(&root_dir.join("state"), JournalOptions::default());
        let (root, child) = unit(&registry, "agent/dispatch-x", Some(&wt));
        seams::arm_after_removal(&wt, || std::thread::sleep(Duration::from_millis(1500)));

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

        let until = std::time::Instant::now() + Duration::from_secs(20);
        while closes(&registry).is_empty() && std::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            closes(&registry),
            vec![(root, CloseHow::Reclaimed), (child, CloseHow::Reclaimed)]
        );
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
