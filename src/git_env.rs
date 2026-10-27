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
//! Production is the narrow claim on purpose. Rule 13 exempts anything gated
//! on `test` by a `#[cfg]`, so a fixture that builds its own repositories is a
//! separate question — [`fixture_git`] below is what those should use, and
//! issue #1121 is where that half is tracked.
//!
//! **The same builders switch off hooks and fsmonitor** (issue #1888). A
//! dispatched agent can write its repository's shared `.git/config` and
//! `.git/hooks/`, which every linked worktree shares, so without this a
//! `post-checkout` hook it planted runs on the deck's next `git worktree add`,
//! and a `core.fsmonitor` command it configured runs on the deck's next `git
//! status`, both in the daemon's process and environment.
//! [`NO_HOOKS_OR_FSMONITOR`] is the list, and every builder below puts it on
//! the command line. The production calls are reads plus `fetch`, `pull
//! --ff-only`, `worktree add`/`remove`/`prune` and `branch -D`, and none of
//! them needs a hook: what they can fire is `post-checkout`, `post-merge` and
//! `reference-transaction`, and Git LFS's copies of the first two only mark
//! lockable files read-only (its content arrives through the smudge filter,
//! which is configuration, not a hook). What the user loses is their own
//! `post-checkout` that bootstraps a fresh worktree, which no longer runs when
//! the deck creates one.

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

/// `-c` overrides that keep the deck's own git commands from running a hook or
/// an fsmonitor command that a repository's configuration or `hooks/`
/// directory names (issue #1888). A `-c` outranks every configuration file,
/// the repository's own included, so neither `core.hooksPath` in
/// `.git/config` nor a script in `.git/hooks/` brings one back.
///
/// Git looks a hook up as `<core.hooksPath>/<name>`, and under `/dev/null` that
/// is never a file on Linux or macOS. Git for Windows is not a target the deck
/// ships for yet; there the path reads as `\dev\null\<name>` on the current
/// drive, which is absent unless someone creates it, so this list is to be
/// revisited before Windows ships.
///
/// The telemetry probe (`crate::telemetry::run_context`) builds on these
/// builders and adds its own list on top, so this list is not repeated there.
pub(crate) const NO_HOOKS_OR_FSMONITOR: [&str; 2] =
    ["core.hooksPath=/dev/null", "core.fsmonitor=false"];

/// `git` with the ambient location environment cleared and
/// [`NO_HOOKS_OR_FSMONITOR`] on the command line — the one body both
/// [`git_at`] and [`git_async`] are built from, so the two cannot drift. The
/// `-c` pairs come first, so they precede whatever subcommand a caller adds.
fn hardened() -> Command {
    let mut cmd = scrubbed();
    for config in NO_HOOKS_OR_FSMONITOR {
        cmd.arg("-c").arg(config);
    }
    cmd
}

/// `git` with only the ambient location environment cleared.
fn scrubbed() -> Command {
    let mut cmd = Command::new(GIT);
    for var in AMBIENT_LOCATION_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// `git`, to be run from inside `dir`, with the ambient location environment
/// switched off so the answer depends on `dir` and nothing else, and with
/// hooks and fsmonitor off ([`NO_HOOKS_OR_FSMONITOR`]).
///
/// Every synchronous `git` invocation in [`crate::worktree_owner`]'s and
/// [`crate::worktree_reclaim`]'s PRODUCTION code goes through here — checked
/// by rule 13, not merely intended — which makes
/// [`crate::worktree_owner::git_dir_of`], and so the ownership gate, the
/// enumeration the reclaim decision is made from, and the removal behind it,
/// immune to the same ambient override. Their test fixtures are outside that
/// claim; see the module header.
pub(crate) fn git_at(dir: &Path) -> Command {
    let mut cmd = hardened();
    cmd.current_dir(dir);
    cmd
}

/// `git`, async, with the ambient location environment switched off and
/// hooks and fsmonitor off — the [`git_at`] of the `tokio` sites.
///
/// No `current_dir`: these callers pass the directory as `-C <dir>` in their
/// argv, which the same variables outrank in exactly the same way (issue
/// #1181's reproduction uses `-C` precisely because that is the shape
/// `issue_dispatch_run` and `dispatch` use).
///
/// Separate from [`git_at`] because `tokio::process::Command` and
/// `std::process::Command` are unrelated types — but it is converted from the
/// same [`hardened`] command, which is the property that has to hold.
pub(crate) fn git_async() -> tokio::process::Command {
    hardened().into()
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
    sandboxed(git_at(dir), sandbox_root)
}

/// Test-only: [`fixture_git`] WITHOUT [`NO_HOOKS_OR_FSMONITOR`], so the
/// repository's hooks and fsmonitor run as they would for a plain `git`. For a
/// control that proves a planted hook is armed before a test shows the deck's
/// own commands do not run it; never for building a fixture.
#[cfg(test)]
pub(crate) fn fixture_git_with_hooks(dir: &Path, sandbox_root: &Path) -> Command {
    let mut cmd = scrubbed();
    cmd.current_dir(dir);
    sandboxed(cmd, sandbox_root)
}

/// The configuration and identity half of [`fixture_git`].
#[cfg(test)]
fn sandboxed(mut cmd: Command, sandbox_root: &Path) -> Command {
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
