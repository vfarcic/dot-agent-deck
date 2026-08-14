//! Repository-state preflight for `cargo xtask linkage-check` (issue #557).
//!
//! A shallow object store breaks history operations across every worktree
//! sharing it, and it does so silently: `git log` works, `git status` is
//! clean, refs resolve to the SHAs you expect. The only symptom shows up
//! later, somewhere else, as `fatal: refusing to merge unrelated histories`
//! — which reads as a wrong or corrupt remote, not as a truncated object
//! store. `.git/shallow` lives in the common dir, so one `git fetch
//! --depth=…` in one linked worktree degrades every worktree in the clone
//! at once. This preflight turns that into a named failure with a remedy at
//! the next `linkage-check` run, instead of an arbitrarily delayed
//! misdiagnosis.
//!
//! It also asserts worktree-registry drift: a `git worktree list` entry
//! whose path no longer exists on disk is the only trace left behind when a
//! worktree is removed without `git worktree prune`, and the degenerate
//! case — the *current* checkout's own entry gone missing — gets its own
//! message rather than leaving whatever runs next to fail with a confusing
//! `no such file or directory`.
//!
//! # The gate
//!
//! Every `actions/checkout@v7` in CI clones at the default depth of 1, so a
//! naive `if shallow { fail }` would fail every job in the matrix. This is
//! resolved structurally, not with an environment-variable escape hatch —
//! an env-gated check is indistinguishable from a passing one and is how a
//! check quietly stops running in the places that matter. The shallow
//! assertion applies only when the repository has more than one worktree,
//! or the current checkout is itself a linked worktree ([`should_assert_shallow`]):
//! the failure mode exists *because* several worktrees share one object
//! store, and a CI runner clones fresh with exactly one, so it is exempt by
//! construction rather than by trusting an environment variable.
//!
//! # Shape
//!
//! [`collect`] is the only part of this module that shells out to git; it
//! turns three git invocations' worth of output into a plain [`RepoState`].
//! Everything that decides pass/fail — [`should_assert_shallow`],
//! [`preflight_failures`], [`is_linked_worktree`], [`parse_worktree_paths`]
//! — is a pure function over already-collected values, so it is unit-tested
//! without building a real git repository.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Named verbatim in the shallow-repository failure message (issue #557):
/// every downstream symptom misdiagnoses as `fatal: refusing to merge
/// unrelated histories`, which reads as a wrong or corrupt remote rather
/// than a truncated one.
const UNSHALLOW_REMEDY: &str = "git fetch --unshallow <remote>";

/// The confirming probe for a shallow repository: returns the ref's own tip
/// (i.e. the tip is parentless) when the history has been truncated.
const SHALLOW_PROBE: &str = "git rev-list --max-parents=0 <ref>";

/// Named verbatim in both worktree-drift failure messages.
const PRUNE_REMEDY: &str = "git worktree prune";

/// One `git worktree list --porcelain` entry: the registered path, and
/// whether it still exists on disk. The existence check is done by the
/// caller ([`collect`]) rather than inside the pure verdict functions below,
/// so drift can be pinned by a test without touching the filesystem.
struct WorktreeEntry {
    path: PathBuf,
    exists: bool,
}

/// Everything [`preflight_failures`] needs, already collected. Kept
/// separate from the git-shelling [`collect`] so the decision logic is pure.
struct RepoState {
    is_shallow: bool,
    worktrees: Vec<WorktreeEntry>,
    /// This checkout's own top-level path (`git rev-parse --show-toplevel`),
    /// matched against `worktrees` by equality for the degenerate case.
    current_worktree: PathBuf,
    /// True when this checkout IS a linked worktree — see
    /// [`is_linked_worktree`].
    is_linked_worktree: bool,
}

/// Whether `--git-common-dir` names a different directory than `--git-dir`
/// — true only for a linked worktree.
///
/// For the primary checkout the two are one directory: measured directly, a
/// plain clone prints `.git` for both (or `../.git` for both, from a
/// subdirectory — relative output tracks cwd identically for both flags, so
/// the comparison still holds). A linked worktree's `--git-dir` is always a
/// `worktrees/<name>` subdirectory of `--git-common-dir` by construction, so
/// the two can never be equal there, and `--git-common-dir` is printed as an
/// absolute path in that case regardless of cwd depth.
fn is_linked_worktree(git_common_dir: &str, git_dir: &str) -> bool {
    git_common_dir != git_dir
}

/// Whether the shallow assertion applies at all (issue #557's gate). CI
/// checkouts are legitimately shallow with exactly one worktree, and a
/// shallow, single-worktree repository cannot yet exhibit the cross-worktree
/// object-store damage this preflight exists to catch — so it is exempt by
/// construction. `is_linked_worktree` is checked independently of the count:
/// it is what we know for certain about *this* checkout even if the
/// registry itself is the thing that turns out to be drifted.
fn should_assert_shallow(state: &RepoState) -> bool {
    state.worktrees.len() > 1 || state.is_linked_worktree
}

/// Parse `git worktree list --porcelain` output into the registered paths,
/// in the order git reports them. Every entry block starts with a
/// `worktree <path>` line; the `HEAD`, `branch`, `bare` and `detached` lines
/// that can follow are not needed here.
fn parse_worktree_paths(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

/// The pure verdict: every failure found against an already-collected
/// [`RepoState`]. Empty means clean.
fn preflight_failures(state: &RepoState) -> Vec<String> {
    let mut failures = Vec::new();

    if should_assert_shallow(state) && state.is_shallow {
        failures.push(format!(
            "repository object store is shallow (`git rev-parse --is-shallow-repository` \
             is true) in a checkout with more than one worktree, or that is itself a linked \
             worktree — every worktree sharing this object store is affected, and it \
             misdiagnoses downstream as `fatal: refusing to merge unrelated histories` rather \
             than a truncated history. Confirm with `{SHALLOW_PROBE}` returning the ref's own \
             tip, then fix with `{UNSHALLOW_REMEDY}`."
        ));
    }

    let missing: Vec<&Path> = state
        .worktrees
        .iter()
        .filter(|w| !w.exists)
        .map(|w| w.path.as_path())
        .collect();
    if !missing.is_empty() {
        let list = missing
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let (does, is) = if missing.len() == 1 {
            ("does", "is")
        } else {
            ("do", "are")
        };
        failures.push(format!(
            "worktree registry drift: {list} {does} no longer exist on disk but {is} still \
             registered — a worktree was removed without pruning. Run `{PRUNE_REMEDY}`."
        ));

        if missing.contains(&state.current_worktree.as_path()) {
            failures.push(format!(
                "this checkout's own worktree, {}, is registered but no longer exists on disk \
                 — whatever runs next will fail with a confusing `no such file or directory` \
                 instead. Run `{PRUNE_REMEDY}`.",
                state.current_worktree.display()
            ));
        }
    }

    failures
}

/// Shells out to git and turns the output into a [`RepoState`]. The only
/// part of this module that is not a pure function.
///
/// Three invocations, kept to that count deliberately (issue #557: "stay
/// cheap"): one combined `rev-parse` for the toplevel, both git-dir flags
/// and the shallow check, plus one `worktree list --porcelain`.
fn collect(root: &Path) -> Result<RepoState, String> {
    let rev_parse = run_git(
        root,
        &[
            "rev-parse",
            "--show-toplevel",
            "--git-common-dir",
            "--git-dir",
            "--is-shallow-repository",
        ],
    )?;
    let mut lines = rev_parse.lines();
    let show_toplevel = lines
        .next()
        .ok_or_else(|| "git rev-parse produced no output".to_string())?;
    let git_common_dir = lines
        .next()
        .ok_or_else(|| "git rev-parse: missing --git-common-dir output".to_string())?;
    let git_dir = lines
        .next()
        .ok_or_else(|| "git rev-parse: missing --git-dir output".to_string())?;
    let is_shallow_raw = lines
        .next()
        .ok_or_else(|| "git rev-parse: missing --is-shallow-repository output".to_string())?;
    let is_shallow = match is_shallow_raw {
        "true" => true,
        "false" => false,
        other => {
            return Err(format!(
                "unexpected --is-shallow-repository output {other:?}"
            ));
        }
    };

    let porcelain = run_git(root, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_worktree_paths(&porcelain)
        .into_iter()
        .map(|path| {
            let exists = path.exists();
            WorktreeEntry { path, exists }
        })
        .collect();

    Ok(RepoState {
        is_shallow,
        worktrees,
        current_worktree: PathBuf::from(show_toplevel),
        is_linked_worktree: is_linked_worktree(git_common_dir, git_dir),
    })
}

fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("failed to invoke `git {}`: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "`git {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim_end().to_string())
        .map_err(|e| format!("`git {}` produced non-UTF-8 output: {e}", args.join(" ")))
}

/// Maps a [`collect`] result to the failures the preflight reports.
///
/// A git invocation that itself fails — not a repository at all, or git
/// missing from `PATH` — is a deliberate PASS, not a failure: every other
/// check in `linkage-check` already depends on the checkout being a working
/// git repository just to read the catalog and the test sources, so a
/// checkout broken badly enough to fail this preflight's git calls will not
/// get further unnoticed. Crashing the whole build because this one
/// preflight could not ask git a question would be worse than skipping it;
/// the reason is still printed, not swallowed.
fn failures_from(collected: Result<RepoState, String>) -> Vec<String> {
    match collected {
        Ok(state) => preflight_failures(&state),
        Err(reason) => {
            eprintln!("xtask linkage-check: repository-state preflight skipped: {reason}");
            Vec::new()
        }
    }
}

/// Entry point `main.rs` calls before the catalog↔test checks. Every
/// failure found (empty means clean).
pub(crate) fn run(root: &Path) -> Vec<String> {
    failures_from(collect(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, exists: bool) -> WorktreeEntry {
        WorktreeEntry {
            path: PathBuf::from(path),
            exists,
        }
    }

    fn state(
        is_shallow: bool,
        worktrees: Vec<WorktreeEntry>,
        current_worktree: &str,
        is_linked_worktree: bool,
    ) -> RepoState {
        RepoState {
            is_shallow,
            worktrees,
            current_worktree: PathBuf::from(current_worktree),
            is_linked_worktree,
        }
    }

    /// The gate this whole change exists for: a CI runner clones fresh with
    /// exactly one worktree and no linked-worktree redirection, so a
    /// shallow object store there is exempt by construction rather than
    /// flagged.
    #[test]
    fn gate_exempts_a_single_worktree_shallow_repo() {
        let s = state(true, vec![entry("/repo", true)], "/repo", false);
        assert!(
            preflight_failures(&s).is_empty(),
            "{:?}",
            preflight_failures(&s)
        );
    }

    /// More than one registered worktree sharing a shallow object store is
    /// exactly the damage shape the preflight exists to catch, and the
    /// message must name the remedy verbatim.
    #[test]
    fn multi_worktree_shallow_repo_fails_and_names_the_remedy() {
        let s = state(
            true,
            vec![entry("/repo", true), entry("/repo-other", true)],
            "/repo",
            false,
        );
        let failures = preflight_failures(&s);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("git fetch --unshallow"),
            "{}",
            failures[0]
        );
        assert!(
            failures[0].contains("git rev-list --max-parents=0"),
            "{}",
            failures[0]
        );
    }

    /// A linked worktree that is shallow fails even when the registry only
    /// lists one entry — `is_linked_worktree` is checked independently of
    /// the count precisely so registry drift cannot mask this case.
    #[test]
    fn a_linked_worktree_that_is_shallow_fails_even_with_one_registered_entry() {
        let s = state(true, vec![entry("/repo/wt", true)], "/repo/wt", true);
        let failures = preflight_failures(&s);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("git fetch --unshallow"),
            "{}",
            failures[0]
        );
    }

    /// A shallow repository that is genuinely exempt AND has no drift stays
    /// silent — the count/linked gate and the drift check are independent.
    #[test]
    fn a_shallow_single_worktree_repo_with_no_drift_is_silent() {
        let s = state(true, vec![entry("/repo", true)], "/repo", false);
        assert!(preflight_failures(&s).is_empty());
    }

    /// Registry drift: an entry whose path is gone is reported with the
    /// path named and the prune remedy, even when the repository is not
    /// shallow at all.
    #[test]
    fn registry_drift_reports_the_missing_path_by_name() {
        let s = state(
            false,
            vec![entry("/repo/main", true), entry("/repo/gone", false)],
            "/repo/main",
            false,
        );
        let failures = preflight_failures(&s);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("/repo/gone"), "{}", failures[0]);
        assert!(
            failures[0].contains("git worktree prune"),
            "{}",
            failures[0]
        );
        // The current worktree is fine, so the degenerate-case message must
        // not also fire.
        assert!(
            !failures[0].contains("this checkout's own worktree"),
            "{}",
            failures[0]
        );
    }

    /// The degenerate case of drift: the CURRENT checkout's own registry
    /// entry is the one that no longer exists on disk. Gets its own
    /// message in addition to the general drift report, rather than
    /// leaving whatever runs next to fail with a confusing `no such file or
    /// directory`.
    #[test]
    fn current_worktree_missing_gets_its_own_message() {
        let s = state(false, vec![entry("/repo/wt", false)], "/repo/wt", true);
        let failures = preflight_failures(&s);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].contains("/repo/wt"), "{}", failures[0]);
        assert!(
            failures[0].contains("git worktree prune"),
            "{}",
            failures[0]
        );
        assert!(
            failures[1].contains("this checkout's own worktree"),
            "{}",
            failures[1]
        );
        assert!(failures[1].contains("/repo/wt"), "{}", failures[1]);
        assert!(
            failures[1].contains("git worktree prune"),
            "{}",
            failures[1]
        );
    }

    /// A healthy multi-worktree, non-shallow repository passes with no
    /// findings at all.
    #[test]
    fn a_healthy_multi_worktree_repo_passes() {
        let s = state(
            false,
            vec![entry("/repo", true), entry("/repo-other", true)],
            "/repo",
            false,
        );
        assert!(preflight_failures(&s).is_empty());
    }

    /// Deliberate decision for the git-unavailable case (issue #557: "fail
    /// closed, carefully"): a `collect` failure — git missing, not a
    /// repository at all — is treated as a PASS rather than crashing the
    /// build, because every other `linkage-check` rule already assumes a
    /// working git checkout to run at all.
    #[test]
    fn a_git_invocation_failure_is_treated_as_a_pass_not_a_crash() {
        let failures = failures_from(Err("git not found on PATH".to_string()));
        assert!(failures.is_empty(), "{failures:?}");
    }

    /// A successful `collect` still runs the real verdict logic through
    /// `failures_from` — the git-unavailable case is the only thing that
    /// short-circuits to empty.
    #[test]
    fn a_successful_collect_still_reports_real_failures() {
        let s = state(
            true,
            vec![entry("/repo", true), entry("/repo-other", true)],
            "/repo",
            false,
        );
        let failures = failures_from(Ok(s));
        assert_eq!(failures.len(), 1, "{failures:?}");
    }

    /// The primary checkout: measured directly, `--git-common-dir` and
    /// `--git-dir` print the identical string, whether relative (from a
    /// subdirectory) or the historical `.git` form — never a linked
    /// worktree.
    #[test]
    fn is_linked_worktree_is_false_for_the_primary_checkout() {
        assert!(!is_linked_worktree(".git", ".git"));
        assert!(!is_linked_worktree("../.git", "../.git"));
        assert!(!is_linked_worktree(
            "/Users/dev/repo/.git",
            "/Users/dev/repo/.git"
        ));
    }

    /// A linked worktree: measured directly, `--git-common-dir` is the
    /// common dir's absolute path and `--git-dir` is its
    /// `worktrees/<name>` subdirectory — always unequal by construction.
    #[test]
    fn is_linked_worktree_is_true_for_a_linked_worktree() {
        assert!(is_linked_worktree(
            "/Users/dev/repo/.git",
            "/Users/dev/repo/.git/worktrees/repo-fix123"
        ));
    }

    #[test]
    fn parse_worktree_paths_extracts_only_the_worktree_lines() {
        let porcelain = "worktree /repo/main\n\
                          HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d\n\
                          branch refs/heads/main\n\
                          \n\
                          worktree /repo/fix-123\n\
                          HEAD 4e402c166433f702351f18e6e26c51205e19df1e\n\
                          branch refs/heads/fix/123-something\n";
        assert_eq!(
            parse_worktree_paths(porcelain),
            vec![PathBuf::from("/repo/main"), PathBuf::from("/repo/fix-123")]
        );
    }

    /// A bare or detached entry omits `branch` in favour of `bare` /
    /// `detached`; the parser only looks for the `worktree ` prefix, so
    /// these shapes cannot break it.
    #[test]
    fn parse_worktree_paths_is_indifferent_to_bare_and_detached_entries() {
        let porcelain = "worktree /repo/bare\n\
                          bare\n\
                          \n\
                          worktree /repo/detached\n\
                          HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d\n\
                          detached\n";
        assert_eq!(
            parse_worktree_paths(porcelain),
            vec![PathBuf::from("/repo/bare"), PathBuf::from("/repo/detached")]
        );
    }

    #[test]
    fn parse_worktree_paths_on_empty_input_is_empty() {
        assert!(parse_worktree_paths("").is_empty());
    }
}
