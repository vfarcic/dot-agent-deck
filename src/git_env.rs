//! The ambient git *location* environment, and the one place this crate
//! switches it off.
//!
//! `GIT_DIR` and its siblings outrank BOTH `-C <dir>` and a command's
//! `current_dir` — so a `git` this crate spawns to act on a directory it chose
//! acts instead on whatever repository the ambient variable names, and reports
//! success. Issue #834 measured that for a test fixture; issue #1181 measured
//! it for production, on the exact command shapes `issue_dispatch_run`,
//! `worktree_reclaim` and `dispatch` use:
//!
//! - **create** — `GIT_DIR=<decoy> git -C <clone> worktree add <wt> -b <branch>`
//!   puts the branch AND the worktree in `decoy`; `clone` is untouched.
//! - **read** — `git worktree list --porcelain` with cwd `clone` enumerates
//!   `decoy`'s worktrees. That is the probe
//!   [`crate::worktree_reclaim::examine_worktrees`] makes its removal decision
//!   from, so a wrong answer here steers a deletion.
//! - **delete** — `git worktree remove <path>` with cwd `clone` removes a
//!   worktree belonging to `decoy` from its metadata and DELETES IT FROM DISK.
//!
//! Reaching it needs a deck that inherited one of these variables, which is
//! what a daemon lazy-spawned from inside a `rebase --exec`, a pre-commit hook
//! or a `bisect run` does.
//!
//! **The list lives here and nowhere else.** A second copy is how a variable
//! goes missing from one of them, and a missing variable is a silent
//! data-loss hole rather than a compile error — so every `git` this crate's
//! PRODUCTION code spawns is built by one of the constructors below, and
//! `xtask/linkage-check`'s rule 13 fails the build on a `git` program literal
//! anywhere else in `src/`'s production half.
//!
//! Production is the narrow claim on purpose. Rule 13 exempts each file's
//! trailing `#[cfg(test)] mod tests`, so a fixture that builds its own
//! repositories is a separate question — [`fixture_git`] below is what those
//! should use, and issue #1121 is where that half is tracked.

use std::path::Path;
use std::process::Command;

/// The environment variables through which git's *location* discovery can be
/// steered from outside this process (issue #834). Every one of them outranks
/// the `current_dir` a command passes — measured in
/// `xtask/linkage-check/src/repo_state.rs`, where an ambient `GIT_DIR` made
/// `git -C <fixture> log` report a different repository's history entirely.
///
/// That matters more for [`crate::worktree_owner`] than it does for a fixture,
/// because the answer is interpolated into the prompt an agent is started
/// with: a daemon lazy-spawned from inside a `rebase --exec`, a pre-commit
/// hook or a `bisect run` carries one of these, and without the scrub every
/// pane it starts would be told to write its durable report into whatever
/// repository that variable named, however unrelated to the pane's own cwd.
/// That is the one outcome that module's fail-closed posture exists to
/// prevent, and it is not a failure a consumer could detect — the variable
/// would be set, and confidently wrong.
///
/// It matters differently, and worse, for the three modules issue #1181
/// covers: there the same override redirects a `worktree add`, a `worktree
/// remove` and a `branch -D` into a repository the deck never chose. The
/// module header has the measurements.
///
/// Cleared rather than overridden, because for each of these "unset" *is*
/// git's default. The list mirrors that file's `AMBIENT_LOCATION_VARS`,
/// including `GIT_DISCOVERY_ACROSS_FILESYSTEM` for the same reason it gives.
///
/// `GIT_CEILING_DIRECTORIES` is deliberately NOT cleared, for a different
/// reason than that file's: it can only *narrow* the upward walk, so an
/// ambient one can make a lookup fail but can never make it resolve a
/// different repository — the fail-closed direction. That argument covers the
/// #1181 sites unchanged: every one of them is handed a repository root or a
/// worktree path, so a ceiling can at worst turn the call into git's own "not
/// a git repository" error, never into a correct-looking action on the wrong
/// repository. Honouring it also leaves an operator's guard against walking a
/// slow network mount in place, on paths that run at every pane spawn.
pub(crate) const AMBIENT_LOCATION_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// The program name, for the error text of callers that report which command
/// failed. It lives here so the literal appears in exactly one place — see the
/// module header on why a second copy of anything in this file is the defect.
pub(crate) const GIT: &str = "git";

/// `git`, to be run from inside `dir`, with the ambient location environment
/// switched off so the answer depends on `dir` and nothing else.
///
/// Every synchronous `git` invocation in [`crate::worktree_owner`]'s and
/// [`crate::worktree_reclaim`]'s PRODUCTION code goes through here — checked
/// by rule 13, not merely intended — which makes
/// [`crate::worktree_owner::git_dir_of`], and so the ownership gate, the
/// enumeration the reclaim decision is made from, and the removal behind it,
/// immune to the same ambient override. Their test fixtures are outside that
/// claim; see the module header.
pub(crate) fn git_at(dir: &Path) -> Command {
    let mut cmd = Command::new(GIT);
    cmd.current_dir(dir);
    for var in AMBIENT_LOCATION_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// `git`, async, with the ambient location environment switched off — the
/// [`git_at`] of the `tokio` sites.
///
/// No `current_dir`: these callers pass the directory as `-C <dir>` in their
/// argv, which the same variables outrank in exactly the same way (issue
/// #1181's reproduction uses `-C` precisely because that is the shape
/// `issue_dispatch_run` and `dispatch` use).
///
/// Separate from [`git_at`] because `tokio::process::Command` and
/// `std::process::Command` are unrelated types — but the variable list they
/// clear is the single one above, which is the property that has to hold.
pub(crate) fn git_async() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(GIT);
    for var in AMBIENT_LOCATION_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// Test-only: `git`, to be run from inside `dir`, with the ambient git
/// environment switched off in all three of the groups
/// `xtask/linkage-check/src/repo_state.rs`'s `Sandbox` documents — so no
/// fixture command can read or WRITE any repository outside `sandbox_root`,
/// including the checkout the tests are running inside.
///
/// Location comes from [`git_at`], plus `GIT_CEILING_DIRECTORIES` bounding the
/// upward walk at the sandbox root — production deliberately leaves that
/// unset, a fixture deliberately sets it. Configuration is neutralized so no
/// developer `~/.gitconfig` (or `includeIf`, or `init.templateDir` hook) reaches
/// a fixture, and the commit identity is supplied by environment rather than by
/// `git config`, so a fixture never writes into a repository to configure one.
///
/// Lives here rather than in any test module because several need it — and
/// because a second copy is exactly how the neutralization drifts out of step
/// with [`git_at`].
#[cfg(test)]
pub(crate) fn fixture_git(dir: &Path, sandbox_root: &Path) -> Command {
    let mut cmd = git_at(dir);
    let absent = sandbox_root.join("no-such-gitconfig");
    cmd.env("GIT_CONFIG_GLOBAL", &absent)
        .env("GIT_CONFIG_SYSTEM", &absent)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", sandbox_root)
        .env("XDG_CONFIG_HOME", sandbox_root)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t.t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t.t")
        .env("GIT_CEILING_DIRECTORIES", sandbox_root);
    cmd
}
