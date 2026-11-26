//! PRD #1790 stage 2: the `git` and `gh` children the interrupted work verbs
//! run (a list's Git and pull request facts, a reclaim's geometry, marker
//! and branch checks), each one bounded.
//!
//! Every child runs through [`crate::issue_dispatch_run::output_bounded`]:
//! its own process group, a deadline, and a cap on the output it may print.
//! At the deadline, or past the cap, the whole group is `SIGKILL`ed and the
//! child reaped before the call returns. A child is started only while one of
//! [`PROBE_CONCURRENCY`] daemon-wide permits is held, and the permit is held
//! until the call returns, so after its child is reaped. Each call takes a
//! [`Deadline`] rather than a duration, so a chain of children (the pull
//! request lookup's four) and a whole list's enrichment share one budget.
//!
//! What this does not cover: a caller that drops a call part-way (an outer
//! timeout) releases the permit when the call is dropped. The group is still
//! `SIGKILL`ed then, synchronously, by the runner's drop guard, but the child
//! is reaped by the runtime afterwards rather than before the permit is free.
//! The verbs here do not drop these calls; each runs to its own deadline. A
//! descendant that moves itself out of the child's process group escapes the
//! kill, as [`crate::issue_dispatch_run::output_bounded`] says.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::interrupted_work::PrFact;

/// How many of these children run at once, across the daemon.
pub const PROBE_CONCURRENCY: usize = 8;

/// The most stdout one child may print before it is killed. Every answer
/// read here is a path, a count, a branch or a short JSON array.
pub const PROBE_STDOUT_CAP: usize = 256 * 1024;

/// The most stderr kept from one child; the rest is read and dropped.
const PROBE_STDERR_CAP: usize = 16 * 1024;

static PERMITS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(PROBE_CONCURRENCY));

/// The instant a probe, or a chain of them, must be done by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(Instant);

impl Deadline {
    /// `bound` from now.
    pub fn after(bound: Duration) -> Self {
        Deadline(Instant::now() + bound)
    }

    /// The earlier of the two.
    pub fn min(self, other: Deadline) -> Self {
        std::cmp::min(self, other)
    }

    /// The same instant, as a [`std::time::Instant`], for a bound that is
    /// not a child's (a walk of a directory, say).
    pub fn as_std(self) -> std::time::Instant {
        self.0.into_std()
    }

    /// The time left, or `None` once it has passed.
    pub fn remaining(self) -> Option<Duration> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }
}

/// Which program a probe runs.
#[derive(Debug, Clone, Copy)]
pub enum Program<'a> {
    /// `git`, hardened against the ambient location environment
    /// ([`crate::git_env::git_async`]).
    Git,
    /// Another program, by name (looked up on `PATH`) or path: `gh`.
    Other(&'a OsStr),
}

/// Run `program` with `args` in `dir` by `deadline`, and answer its stdout
/// when it exits zero. `Err` when no permit came free in time, the deadline
/// passed, it printed more than [`PROBE_STDOUT_CAP`], it could not start, or
/// it exited non-zero.
pub async fn run(
    program: Program<'_>,
    dir: &Path,
    args: &[&str],
    deadline: Deadline,
) -> Result<Vec<u8>, String> {
    let out_of_time = || "out of time".to_string();
    let wait = deadline.remaining().ok_or_else(out_of_time)?;
    let _permit = tokio::time::timeout(wait, PERMITS.acquire())
        .await
        .map_err(|_| out_of_time())?
        .map_err(|e| e.to_string())?;
    let bound = deadline.remaining().ok_or_else(out_of_time)?;
    let (mut cmd, name) = match program {
        Program::Git => (crate::git_env::git_async(), crate::git_env::GIT.to_string()),
        Program::Other(name) => (
            tokio::process::Command::new(name),
            name.to_string_lossy().into_owned(),
        ),
    };
    cmd.current_dir(dir);
    let output = crate::issue_dispatch_run::output_bounded(
        cmd,
        &name,
        args,
        bound,
        PROBE_STDOUT_CAP,
        PROBE_STDERR_CAP,
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "`{name} {}` failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// [`run`] for `git`, its stdout as text.
pub async fn git(dir: &Path, args: &[&str], deadline: Deadline) -> Result<String, String> {
    run(Program::Git, dir, args, deadline)
        .await
        .map(|out| String::from_utf8_lossy(&out).into_owned())
}

/// `git rev-parse <flag>` from inside `dir`, read as
/// [`crate::worktree_owner::rev_parse_answer`] reads it.
pub async fn rev_parse(dir: &Path, flag: &str, deadline: Deadline) -> Option<PathBuf> {
    let out = run(Program::Git, dir, &["rev-parse", flag], deadline)
        .await
        .ok()?;
    crate::worktree_owner::rev_parse_answer(dir, &out)
}

/// What a reclaim reads about a directory before trusting it as a worktree
/// the deck made: its verified geometry and its git metadata directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeFacts {
    pub geometry: crate::worktree_owner::VerifiedGeometry,
    /// `git rev-parse --git-dir`, canonical: where the owner marker lives.
    pub git_dir: PathBuf,
}

/// [`crate::worktree_owner::verified_geometry_of`] with its three `git`
/// answers asked under `deadline`, and its filesystem checks waited for
/// until `deadline` too; `None` when any is not answered in time or the
/// checks fail.
pub async fn worktree_facts(dir: &Path, deadline: Deadline) -> Option<WorktreeFacts> {
    let (toplevel, git_dir, common) = tokio::join!(
        rev_parse(dir, "--show-toplevel", deadline),
        rev_parse(dir, "--git-dir", deadline),
        rev_parse(dir, "--git-common-dir", deadline),
    );
    let (toplevel, git_dir, common) = (toplevel?, git_dir?, common?);
    // The checks read the filesystem on the blocking pool, each read bounded
    // (the back-pointer at `MAX_BACK_POINTER_BYTES`). The deadline bounds
    // the wait for them: past it the answer is `None`. That does not cancel
    // the blocking task, which finishes its bounded reads on its own.
    let wait = deadline.remaining()?;
    let checks = tokio::task::spawn_blocking(move || {
        let geometry = crate::worktree_owner::verify_geometry(&toplevel, &git_dir, &common)?;
        let git_dir = std::fs::canonicalize(&git_dir).ok()?;
        Some(WorktreeFacts { geometry, git_dir })
    });
    tokio::time::timeout(wait, checks)
        .await
        .ok()?
        .ok()
        .flatten()
}

/// The branch checked out in `worktree`, or `None` when `HEAD` is detached
/// or Git did not answer in time.
pub async fn head_branch(worktree: &Path, deadline: Deadline) -> Option<String> {
    git(
        worktree,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        deadline,
    )
    .await
    .ok()
    .map(|out| out.trim().to_string())
    .filter(|branch| !branch.is_empty())
}

/// How many commits `HEAD` has that no remote-tracking branch holds.
pub async fn unpushed_commits(worktree: &Path, deadline: Deadline) -> Option<u64> {
    git(
        worktree,
        &["rev-list", "--count", "HEAD", "--not", "--remotes"],
        deadline,
    )
    .await
    .ok()?
    .trim()
    .parse()
    .ok()
}

/// Whether `branch` is a local branch of `worktree`'s repository.
pub async fn branch_exists(worktree: &Path, branch: &str, deadline: Deadline) -> Option<bool> {
    let full = format!("refs/heads/{branch}");
    let out = git(
        worktree,
        &["for-each-ref", "--format=%(refname)", &full],
        deadline,
    )
    .await
    .ok()?;
    Some(out.lines().any(|line| line == full))
}

/// Whether `git status --porcelain` reports uncommitted changes in
/// `worktree`, as [`crate::issue_dispatch_run::worktree_is_dirty`] asks it.
/// Output past [`PROBE_STDOUT_CAP`] kills the child, and is still an answer:
/// that much output is uncommitted changes.
pub async fn is_dirty(worktree: &Path, deadline: Deadline) -> Option<bool> {
    match git(worktree, &["status", "--porcelain"], deadline).await {
        Ok(out) => Some(!out.trim().is_empty()),
        Err(e) if crate::issue_dispatch_run::is_subprocess_overflow(&e) => Some(true),
        Err(_) => None,
    }
}

/// The state of `branch`'s pull request, as
/// [`crate::worktree_reclaim::resolve_pr_state`] reads it, with every `git`
/// and `gh` child it runs bounded by `deadline`. `gh` is the program to run
/// for it (`gh` on `PATH` in production). [`PrFact::Unknown`] whenever any
/// step fails or runs out of time, the two lookups of where the branch is
/// pushed included; [`PrFact::None`] only when every step answered and no
/// pull request is the branch's.
pub async fn pr_state(repo_dir: &Path, branch: &str, gh: &OsStr, deadline: Deadline) -> PrFact {
    use crate::worktree_reclaim::{
        GH_PR_FIELDS, PrState, parse_github_owner_repo, parse_pr_state, parse_push_remote,
        push_remote_args, push_repos_from_urls,
    };
    let Ok(origin) = git(repo_dir, &["remote", "get-url", "origin"], deadline).await else {
        return PrFact::Unknown;
    };
    let Some(slug) = parse_github_owner_repo(origin.trim()) else {
        return PrFact::Unknown;
    };
    let args = crate::pull_request::gh_pr_list_args(branch, &slug, GH_PR_FIELDS);
    let Ok(listed) = run(Program::Other(gh), repo_dir, &args, deadline).await else {
        return PrFact::Unknown;
    };
    // Which repositories the branch is pushed to decides whether a fork's
    // pull request is the branch's own, so a lookup that fails is not read
    // as "pushed nowhere" (reviewer 3): only a lookup that succeeds and finds
    // no push remote leaves the set empty.
    let branch_ref = format!("refs/heads/{branch}");
    let Ok(refs) = git(repo_dir, &push_remote_args(&branch_ref), deadline).await else {
        return PrFact::Unknown;
    };
    let push_repos = match parse_push_remote(&refs, &branch_ref) {
        Some(remote) => {
            let Ok(urls) = git(
                repo_dir,
                &["remote", "get-url", "--push", "--all", &remote],
                deadline,
            )
            .await
            else {
                return PrFact::Unknown;
            };
            push_repos_from_urls(&urls)
        }
        None => Vec::new(),
    };
    // A tip that cannot be read leaves a merged or closed pull request
    // unknown in `parse_pr_state`, which reads it as unresolvable.
    let tip_args = crate::pull_request::branch_tip_args(branch);
    let tip_args: Vec<&str> = tip_args.iter().map(String::as_str).collect();
    let tip = git(repo_dir, &tip_args, deadline)
        .await
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|tip| !tip.is_empty());
    match parse_pr_state(
        &String::from_utf8_lossy(&listed),
        branch,
        &push_repos,
        tip.as_deref(),
    ) {
        PrState::Open => PrFact::Open,
        PrState::Merged => PrFact::Merged,
        PrState::ClosedUnmerged => PrFact::Closed,
        PrState::NoPr => PrFact::None,
        PrState::Unresolvable(_) => PrFact::Unknown,
    }
}

#[cfg(all(test, unix))]
#[path = "interrupted_work_probe_tests.rs"]
mod tests;
