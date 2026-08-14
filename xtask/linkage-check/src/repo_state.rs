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
//! turns two git invocations' worth of output into a plain [`RepoState`].
//! Everything that decides pass/fail — [`should_assert_shallow`],
//! [`preflight_failures`], [`is_linked_worktree`], [`parse_worktree_entries`],
//! [`parse_rev_parse`] — is a pure function over already-collected values, so
//! it is unit-tested without building a real git repository.

use std::ffi::OsString;
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

/// One `git worktree list --porcelain` entry: the registered path, whether
/// it still exists on disk, and git's own `prunable` reason when it already
/// told us (see [`parse_worktree_entries`]). The existence check is done by
/// the caller ([`collect`]) rather than inside the pure verdict functions
/// below, so drift can be pinned by a test without touching the filesystem.
#[derive(Debug)]
struct WorktreeEntry {
    path: PathBuf,
    exists: bool,
    /// `Some(reason)` when `git worktree list --porcelain` already reported
    /// this entry `prunable <reason>` (git ≥ 2.36) — folded into the drift
    /// message instead of composing our own explanation. `None` means
    /// either a genuinely healthy entry or an older git that predates the
    /// attribute; `exists` falls back to `Path::try_exists()` in that case.
    prunable_reason: Option<String>,
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
/// This is sound BECAUSE [`collect`] always passes `--path-format=absolute`,
/// not merely because it currently happens to be invoked from the git
/// toplevel. Measured directly on git 2.55.0: WITHOUT `--path-format`, from
/// a subdirectory of a primary checkout, `--git-common-dir` stays relative
/// (`../../.git`) while `--git-dir` is printed absolute
/// (`/abs/.../.git`) — the two compare unequal, so this function would
/// wrongly return `true` for a primary checkout. That is the dangerous
/// direction: it would turn the shallow assertion on for a fresh, shallow,
/// single-worktree CI clone, which is exactly the outcome
/// [`should_assert_shallow`]'s exemption exists to prevent. With
/// `--path-format=absolute`, both flags are forced absolute regardless of
/// cwd depth, and a linked worktree's `--git-dir` is always a
/// `worktrees/<name>` subdirectory of `--git-common-dir` by construction —
/// so the two are equal for the primary checkout and unequal for a linked
/// one, always, not just when `collect`'s caller happens to invoke it from
/// the toplevel.
fn is_linked_worktree(git_common_dir: &Path, git_dir: &Path) -> bool {
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

/// Builds an [`OsString`] from raw git output bytes without the hard UTF-8
/// failure `String::from_utf8` would give: a single non-UTF-8 byte in one
/// worktree's path (legal on Linux — ext4/xfs accept any byte but `/` and
/// NUL) must degrade only that path, not the whole preflight, including the
/// shallow assertion that has nothing to do with paths. Unix keeps the
/// bytes exactly (`OsStrExt::from_bytes`), so equality comparisons
/// ([`is_linked_worktree`], the degenerate-case match in
/// [`preflight_failures`]) stay byte-exact instead of corrupted by a lossy
/// round-trip; non-Unix falls back to lossy UTF-8 — this crate's existing
/// idiom elsewhere (`list_tests.rs`) — which is moot in practice since
/// `cargo xtask linkage-check` runs in CI on Linux only.
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes).to_os_string()
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// One `git worktree list --porcelain` entry as parsed, before the
/// filesystem existence check ([`collect`] does that).
#[derive(Debug)]
struct ParsedWorktree {
    path: PathBuf,
    prunable_reason: Option<String>,
}

/// Parse `git worktree list --porcelain` output into per-entry records, in
/// the order git reports them. Every block starts with a `worktree <path>`
/// line; `HEAD`, `branch`, `bare`, `detached` and `locked` lines are
/// ignored, and a `prunable <reason>` line (git ≥ 2.36) attaches to the
/// block it appears in — preferred over re-deriving drift with
/// `Path::exists()`/`try_exists()` in [`collect`] because it survives a
/// corrupted `path` field: git accepts a literal newline inside a worktree
/// path and porcelain emits it unescaped, splitting one entry's block
/// across two lines. `path` is truncated at the newline in that case, but
/// the `prunable` line — several lines later in the SAME block — is
/// unaffected, so keying off it instead of the mangled path is what makes a
/// genuinely-removed worktree with a newline in its path still report as
/// drift rather than fail green. Any line that starts none of the
/// recognised prefixes — including the tail end of a newline-split path —
/// is silently skipped rather than treated as a new entry, since only a
/// `worktree ` line ever starts one.
fn parse_worktree_entries(porcelain: &[u8]) -> Vec<ParsedWorktree> {
    let mut out = Vec::new();
    let mut current: Option<ParsedWorktree> = None;
    for line in porcelain.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"worktree ") {
            if let Some(entry) = current.take() {
                out.push(entry);
            }
            current = Some(ParsedWorktree {
                path: PathBuf::from(os_string_from_bytes(rest)),
                prunable_reason: None,
            });
        } else if let Some(rest) = line.strip_prefix(b"prunable ")
            && let Some(entry) = current.as_mut()
        {
            entry.prunable_reason = Some(String::from_utf8_lossy(rest).into_owned());
        }
    }
    if let Some(entry) = current.take() {
        out.push(entry);
    }
    out
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

    let missing: Vec<&WorktreeEntry> = state.worktrees.iter().filter(|w| !w.exists).collect();
    if !missing.is_empty() {
        // `{:?}` rather than `.display()`: porcelain preserves control
        // characters verbatim, and `Debug` for `Path` escapes them so a
        // trailing-space, `\r` or ANSI-escape path is visible in the
        // message instead of corrupting the terminal or CI log it lands in.
        let list = missing
            .iter()
            .map(|w| match &w.prunable_reason {
                Some(reason) => format!("{:?} ({reason})", w.path),
                None => format!("{:?}", w.path),
            })
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

        if missing.iter().any(|w| w.path == state.current_worktree) {
            failures.push(format!(
                "this checkout's own worktree, {:?}, is registered but no longer exists on disk \
                 — whatever runs next will fail with a confusing `no such file or directory` \
                 instead. Run `{PRUNE_REMEDY}`.",
                state.current_worktree
            ));
        }
    }

    failures
}

/// What [`parse_rev_parse`] reads out of the combined `rev-parse` output.
#[derive(Debug, PartialEq)]
struct RevParse {
    is_shallow: bool,
    current_worktree: PathBuf,
    is_linked_worktree: bool,
    /// True when the path fields could not be trusted because one of them
    /// contained a literal newline — see [`parse_rev_parse`]. `is_shallow`
    /// is still exact; `is_linked_worktree` has been forced to the
    /// fail-safe value.
    paths_degraded: bool,
}

/// Positional parse of the combined `rev-parse` output.
///
/// **`--is-shallow-repository` is requested FIRST, and the order is
/// load-bearing** (Greptile P1 on PR #558). git accepts a literal newline
/// inside a worktree path, and `rev-parse` emits path values unescaped, so
/// a newline in the current checkout's own path splits one value across two
/// lines and shifts every field after it. When the shallow flag was
/// requested last it absorbed that shift, landed on a path instead of
/// `true`/`false`, and returned `Err` — which [`failures_from`] turns into
/// a **silent skip of the entire preflight**, shallow assertion included.
/// Measured against real git 2.55.0 in a worktree whose path contains a
/// newline: five output lines, not four. That is a fail-green in exactly
/// the configuration the preflight is supposed to be unconditionally active
/// in — a linked worktree.
///
/// Asking for the fixed-shape value first makes it line 0, where no amount
/// of path weirdness can displace it. The path fields can still shift, so
/// when more of them arrive than were asked for, this reports
/// `is_linked_worktree = true` rather than comparing two mismatched
/// strings: that is the fail-SAFE direction (it only makes
/// [`should_assert_shallow`] assert more), where the old behaviour failed
/// green. This is the same principle the byte-level handling already
/// follows — one odd path degrades that path, never the shallow assertion,
/// which has nothing to do with paths.
///
/// A trailing `\r` is stripped per line: the previous `String`-based
/// implementation went through `str::lines()`, which strips it, and the
/// move to a byte split silently dropped that. Without it, CRLF output
/// would leave every field with a trailing `\r` and make
/// [`is_linked_worktree`] compare unequal for a primary checkout — the same
/// false positive `--path-format=absolute` exists to prevent.
fn parse_rev_parse(stdout: &[u8]) -> Result<RevParse, String> {
    let lines: Vec<&[u8]> = stdout
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();

    let is_shallow = match lines.first().copied() {
        None => return Err("git rev-parse produced no output".to_string()),
        Some(b"true") => true,
        Some(b"false") => false,
        Some(other) => {
            return Err(format!(
                "unexpected --is-shallow-repository output {:?}",
                String::from_utf8_lossy(other)
            ));
        }
    };

    // `--show-toplevel --git-common-dir --git-dir`, in that order.
    let paths = &lines[1..];
    if paths.len() < 3 {
        return Err(format!(
            "git rev-parse: expected 3 path values after --is-shallow-repository, got {}",
            paths.len()
        ));
    }
    let current_worktree = PathBuf::from(os_string_from_bytes(paths[0]));
    if paths.len() > 3 {
        return Ok(RevParse {
            is_shallow,
            current_worktree,
            is_linked_worktree: true,
            paths_degraded: true,
        });
    }
    let git_common_dir = PathBuf::from(os_string_from_bytes(paths[1]));
    let git_dir = PathBuf::from(os_string_from_bytes(paths[2]));
    Ok(RevParse {
        is_shallow,
        current_worktree,
        is_linked_worktree: is_linked_worktree(&git_common_dir, &git_dir),
        paths_degraded: false,
    })
}

/// Shells out to git and turns the output into a [`RepoState`]. The only
/// part of this module that is not a pure function.
///
/// Two invocations, kept to that count deliberately (issue #557: "stay
/// cheap"): one combined `rev-parse` for the shallow check, the toplevel
/// and both git-dir flags, plus one `worktree list --porcelain`.
/// `--path-format=absolute` is what makes [`is_linked_worktree`]'s
/// comparison sound rather than incidental — see its doc comment — and the
/// **argument order** is what keeps a newline in a path from disabling the
/// whole preflight; see [`parse_rev_parse`].
fn collect(root: &Path) -> Result<RepoState, String> {
    let rev_parse = run_git(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--is-shallow-repository",
            "--show-toplevel",
            "--git-common-dir",
            "--git-dir",
        ],
    )?;
    let parsed = parse_rev_parse(&rev_parse)?;
    if parsed.paths_degraded {
        eprintln!(
            "linkage-check: repository-state preflight: a git-reported path contains a newline; \
             treating this checkout as a linked worktree so the shallow assertion still applies."
        );
    }

    let porcelain = run_git(root, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_worktree_entries(&porcelain)
        .into_iter()
        .map(|entry| {
            // Unreadable (e.g. EACCES on a live worktree under an unreadable
            // parent) is not evidence of drift — degrade to "present"
            // rather than the false positive `Path::exists()` gives here
            // (it returns `false` on ANY error, permission denied
            // included).
            //
            // `paths_degraded` gets the same treatment for the same reason.
            // A newline in a git-reported path truncates the `path` field
            // here exactly as it does in `rev-parse`, so `try_exists()`
            // would be asking about a path that was never on disk and would
            // report a live worktree as drifted. git's own `prunable`
            // verdict is unaffected by the mangling — that is why drift
            // keys off it — so when the path text cannot be trusted, trust
            // only what git said.
            let exists = match &entry.prunable_reason {
                Some(_) => false,
                None if parsed.paths_degraded => true,
                None => entry.path.try_exists().unwrap_or(true),
            };
            WorktreeEntry {
                path: entry.path,
                exists,
                prunable_reason: entry.prunable_reason,
            }
        })
        .collect();

    Ok(RepoState {
        is_shallow: parsed.is_shallow,
        worktrees,
        current_worktree: parsed.current_worktree,
        is_linked_worktree: parsed.is_linked_worktree,
    })
}

/// Runs `git` and returns raw stdout bytes (trailing newline/carriage
/// return trimmed). No UTF-8 validation here — see
/// [`os_string_from_bytes`] for why a hard UTF-8 failure on one path must
/// not disable the whole preflight.
fn run_git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
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
    let mut stdout = output.stdout;
    while matches!(stdout.last(), Some(b'\n') | Some(b'\r')) {
        stdout.pop();
    }
    Ok(stdout)
}

/// Maps a [`collect`] result to the failures the preflight reports.
///
/// A git invocation that itself fails is a deliberate PASS, not a failure —
/// but the bucket routed here is wider than "not a repository, or git
/// missing from `PATH`". It also covers: a **bare** repository
/// (`--show-toplevel` fails outright there); `safe.directory` / "detected
/// dubious ownership" refusals, which a container or CI runner executing as
/// a different uid than the checkout's owner hits on an otherwise perfectly
/// healthy repository — an ordinary state, not an exotic one; any other
/// non-zero git exit; and an unrecognised `rev-parse` flag on an older git,
/// which git echoes verbatim to stdout while exiting 0, failing
/// [`parse_rev_parse`] and routing here too. `collect` now
/// requires git ≥ 2.31 (`--path-format`, added that release) — the higher
/// of the two floors in play, since `--is-shallow-repository` alone only
/// needs ≥ 2.15; below 2.31 the whole preflight silently skips rather than
/// announcing the version gap.
///
/// The reason to fail open: crashing the whole build because this one
/// preflight could not ask git a question would be worse than skipping it.
/// The reason is NOT — as this comment used to claim — that every other
/// `linkage-check` rule already depends on git and would therefore catch a
/// broken checkout anyway. It would not: every one of the eight catalog
/// rules reads `tests/CATALOG.md` and the test sources off the filesystem,
/// not through git, so a checkout where git fails but files are readable —
/// exactly the `dubious ownership` case — runs the whole suite to a green
/// exit with this preflight silently absent. The reason is still printed,
/// not swallowed, in a `linkage-check:`-prefixed block shaped like the
/// failure path's in `main.rs`, so a skip is not the one outcome with no
/// machine-readable trace.
fn failures_from(collected: Result<RepoState, String>) -> Vec<String> {
    match collected {
        Ok(state) => preflight_failures(&state),
        Err(reason) => {
            eprintln!(
                "linkage-check: repository-state preflight: skipped (git unavailable or not usable):"
            );
            eprintln!("  {reason}");
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
            prunable_reason: None,
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

    /// git's own `prunable` reason, when present, is folded into the drift
    /// message instead of a message we compose from scratch.
    #[test]
    fn registry_drift_message_includes_gits_own_reason_when_present() {
        let s = state(
            false,
            vec![WorktreeEntry {
                path: PathBuf::from("/repo/gone"),
                exists: false,
                prunable_reason: Some("gitdir file points to non-existent location".to_string()),
            }],
            "/repo/main",
            false,
        );
        let failures = preflight_failures(&s);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("gitdir file points to non-existent location"),
            "{}",
            failures[0]
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
    /// repository at all, a bare repo, dubious ownership, an old git — is
    /// treated as a PASS rather than crashing the build. See
    /// [`failures_from`]'s doc comment for why, and for why that is NOT
    /// because another `linkage-check` rule would catch the same breakage.
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

    /// The primary checkout: measured directly against git 2.55.0 with
    /// `--path-format=absolute` (which [`collect`] always passes), both
    /// dir flags print the identical absolute path — from the toplevel AND
    /// from a subdirectory. Unlike before this fix, there is no
    /// relative-form fixture here: with `--path-format=absolute` in play, a
    /// relative pair is not a shape git can produce, so pinning one would
    /// pin a fiction again (fork PR #558 review, F1/F2).
    #[test]
    fn is_linked_worktree_is_false_for_the_primary_checkout() {
        assert!(!is_linked_worktree(
            Path::new("/Users/dev/repo/.git"),
            Path::new("/Users/dev/repo/.git")
        ));
    }

    /// A linked worktree: measured directly, `--git-common-dir` is the
    /// common dir's absolute path and `--git-dir` is its
    /// `worktrees/<name>` subdirectory — always unequal by construction,
    /// from the worktree's own toplevel AND from a subdirectory of it
    /// (verified at both depths against real git 2.55.0).
    #[test]
    fn is_linked_worktree_is_true_for_a_linked_worktree() {
        assert!(is_linked_worktree(
            Path::new("/Users/dev/repo/.git"),
            Path::new("/Users/dev/repo/.git/worktrees/repo-fix123")
        ));
    }

    #[test]
    fn parse_worktree_entries_extracts_only_the_worktree_lines() {
        let porcelain = "worktree /repo/main\n\
                          HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d\n\
                          branch refs/heads/main\n\
                          \n\
                          worktree /repo/fix-123\n\
                          HEAD 4e402c166433f702351f18e6e26c51205e19df1e\n\
                          branch refs/heads/fix/123-something\n";
        let paths: Vec<PathBuf> = parse_worktree_entries(porcelain.as_bytes())
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("/repo/main"), PathBuf::from("/repo/fix-123")]
        );
    }

    /// A bare or detached entry omits `branch` in favour of `bare` /
    /// `detached`; the parser only looks for the `worktree ` prefix, so
    /// these shapes cannot break it.
    #[test]
    fn parse_worktree_entries_is_indifferent_to_bare_and_detached_entries() {
        let porcelain = "worktree /repo/bare\n\
                          bare\n\
                          \n\
                          worktree /repo/detached\n\
                          HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d\n\
                          detached\n";
        let paths: Vec<PathBuf> = parse_worktree_entries(porcelain.as_bytes())
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("/repo/bare"), PathBuf::from("/repo/detached")]
        );
    }

    #[test]
    fn parse_worktree_entries_on_empty_input_is_empty() {
        assert!(parse_worktree_entries(b"").is_empty());
    }

    /// git ≥ 2.36 reports drift itself; the `prunable` line attaches to the
    /// block it appears in, and an entry with none gets `None` (the
    /// [`collect`] caller falls back to `Path::try_exists()` for those).
    #[test]
    fn parse_worktree_entries_attaches_prunable_reason_to_its_block() {
        let porcelain = "worktree /repo/main\n\
                          HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d\n\
                          branch refs/heads/main\n\
                          \n\
                          worktree /repo/gone\n\
                          HEAD 4e402c166433f702351f18e6e26c51205e19df1e\n\
                          branch refs/heads/gone\n\
                          prunable gitdir file points to non-existent location\n";
        let entries = parse_worktree_entries(porcelain.as_bytes());
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].prunable_reason, None);
        assert_eq!(
            entries[1].prunable_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );
    }

    /// Reproduces the fail-green case a security audit found in the first
    /// version of this preflight (fork PR #558 review, auditor F1): git
    /// accepts a literal newline inside a worktree path, and `--porcelain`
    /// emits it unescaped, splitting one entry's block across two lines —
    /// `junk` here is the tail of the real path, not a new entry, since it
    /// does not start with `worktree `. The `path` field this parser
    /// produces is therefore truncated and wrong, but the `prunable` line
    /// several lines later is still correctly attached to the SAME block —
    /// which is what lets [`collect`] report the drift correctly even
    /// though the path used for display is mangled. Byte-for-byte against
    /// real `git worktree list --porcelain` output.
    #[test]
    fn parse_worktree_entries_still_flags_drift_when_the_path_contains_a_newline() {
        let porcelain = "worktree /repo/green/keep\n\
                          junk\n\
                          HEAD 1fb69e17f2d79d384969cdb648969ab3459524da\n\
                          branch refs/heads/sneaky\n\
                          prunable gitdir file points to non-existent location\n";
        let entries = parse_worktree_entries(porcelain.as_bytes());
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].path, PathBuf::from("/repo/green/keep"));
        assert_eq!(
            entries[0].prunable_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );
    }

    /// The ordinary shape: shallow flag first, then the three path values.
    #[test]
    fn parse_rev_parse_reads_the_primary_checkout() {
        let out = b"false\n/repo\n/repo/.git\n/repo/.git" as &[u8];
        let parsed = parse_rev_parse(out).expect("parses");
        assert_eq!(
            parsed,
            RevParse {
                is_shallow: false,
                current_worktree: PathBuf::from("/repo"),
                is_linked_worktree: false,
                paths_degraded: false,
            }
        );
    }

    /// A linked worktree: `--git-dir` is the `worktrees/<name>`
    /// subdirectory, so the two dir flags compare unequal.
    #[test]
    fn parse_rev_parse_reads_a_linked_worktree() {
        let out = b"true\n/repo/wt\n/repo/.git\n/repo/.git/worktrees/wt" as &[u8];
        let parsed = parse_rev_parse(out).expect("parses");
        assert!(parsed.is_shallow);
        assert!(parsed.is_linked_worktree);
        assert!(!parsed.paths_degraded);
    }

    /// **The Greptile P1 regression test.** Byte-for-byte the output real
    /// git 2.55.0 produced from a linked worktree whose path contains a
    /// literal newline, with `--is-shallow-repository` requested FIRST:
    /// five lines instead of four. Before the reorder the shallow flag was
    /// last, absorbed the shift, landed on a path instead of
    /// `true`/`false`, and returned `Err` — which `failures_from` turns
    /// into a silent skip of the WHOLE preflight. Now the flag is exact,
    /// nothing returns `Err`, and the untrustworthy path comparison
    /// degrades to the fail-safe `is_linked_worktree = true` instead of the
    /// old fail-green.
    #[test]
    fn parse_rev_parse_survives_a_newline_inside_a_path() {
        let out = b"false\n/probe/wt\nnewline\n/probe/origin/.git\n\
                    /probe/origin/.git/worktrees/wt-newline" as &[u8];
        let parsed = parse_rev_parse(out).expect("must NOT be an Err — Err means a silent skip");
        assert!(!parsed.is_shallow, "the shallow flag must still be exact");
        assert!(
            parsed.is_linked_worktree,
            "must degrade to the fail-safe direction, not fail green"
        );
        assert!(parsed.paths_degraded);
    }

    /// The same shift, in a repository that IS shallow: the whole point of
    /// the reorder is that this case now reports the shallow failure rather
    /// than skipping silently.
    #[test]
    fn a_shallow_repo_with_a_newline_path_still_reports_the_shallow_failure() {
        let out = b"true\n/probe/wt\nnewline\n/probe/origin/.git\n\
                    /probe/origin/.git/worktrees/wt-newline" as &[u8];
        let parsed = parse_rev_parse(out).expect("parses");
        let s = state(
            parsed.is_shallow,
            vec![entry("/probe/wt", true)],
            "/probe/wt",
            parsed.is_linked_worktree,
        );
        let failures = preflight_failures(&s);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("git fetch --unshallow"),
            "{}",
            failures[0]
        );
    }

    /// A trailing `\r` per line is stripped. `str::lines()` used to do this
    /// for free; the byte split does not, and without it CRLF output would
    /// make the two dir flags compare unequal for a primary checkout — the
    /// same false positive `--path-format=absolute` exists to prevent
    /// (fork PR #558 review, R2-2).
    #[test]
    fn parse_rev_parse_strips_carriage_returns() {
        let out = b"false\r\n/repo\r\n/repo/.git\r\n/repo/.git" as &[u8];
        let parsed = parse_rev_parse(out).expect("parses");
        assert_eq!(parsed.current_worktree, PathBuf::from("/repo"));
        assert!(
            !parsed.is_linked_worktree,
            "a stray \\r must not read as a linked worktree"
        );
    }

    /// An older git echoes an unrecognised flag verbatim to stdout while
    /// exiting 0. That is not `true`/`false`, so it is an honest `Err` —
    /// routed to the documented fail-open skip rather than guessed at.
    #[test]
    fn parse_rev_parse_rejects_output_that_is_not_the_shallow_flag() {
        let out = b"--path-format=absolute\n/repo\n/repo/.git\n/repo/.git" as &[u8];
        assert!(parse_rev_parse(out).is_err());
    }

    #[test]
    fn parse_rev_parse_rejects_truncated_output() {
        assert!(parse_rev_parse(b"false\n/repo").is_err());
    }

    /// A single non-UTF-8 byte in a git-reported path (legal on Linux;
    /// ext4/xfs accept any byte but `/` and NUL) must not corrupt the path
    /// through a lossy round-trip — that would break the byte-exact equality
    /// [`is_linked_worktree`] and the degenerate-case match in
    /// [`preflight_failures`] depend on, and `String::from_utf8` would hard-
    /// fail the whole preflight over one unrelated worktree's odd byte
    /// (fork PR #558 review, auditor F4). Unix-only: [`os_string_from_bytes`]
    /// falls back to lossy UTF-8 off Unix, where this preflight does not run
    /// in CI anyway.
    #[cfg(unix)]
    #[test]
    fn os_string_from_bytes_preserves_non_utf8_bytes_exactly() {
        use std::os::unix::ffi::OsStrExt;
        let bytes = [b'/', b'r', b'e', b'p', 0xFF, b'o'];
        let os = os_string_from_bytes(&bytes);
        assert_eq!(os.as_bytes(), &bytes[..]);
    }
}
