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
//! [`run_git`] is the only part of this module that shells out to git.
//! [`collect_with`] turns two git invocations' worth of output into a plain
//! [`RepoState`], and [`collect`] is the four-line binding of the two.
//! Everything that decides pass/fail — [`should_assert_shallow`],
//! [`preflight_failures`], [`is_linked_worktree`], [`parse_worktree_entries`],
//! [`parse_rev_parse`] — is a pure function over already-collected values, so
//! it is unit-tested without building a real git repository.
//!
//! # How it is tested (issue #568)
//!
//! Three layers, because no single one of them reaches everything:
//!
//! - **`mod tests`** — the pure layer, over hand-built values. Fast, exact,
//!   and able to pin shapes real git will not produce on demand (a `\r`, a
//!   non-UTF-8 byte, a git < 2.36 record layout).
//! - **`mod fake_git`** — [`collect_with`] driven by a recording stub. This
//!   is the only layer that can see the git *argument list* and the `-z` →
//!   `--porcelain` fallback, neither of which has any observable effect on a
//!   healthy repository, and the second of which cannot be provoked at all on
//!   a git new enough to run these tests.
//! - **`mod real_git`** — [`collect`] and [`run`] against repositories built
//!   in a `tempfile::tempdir()` by real `git`: a shallow clone, a linked
//!   worktree, registry drift, a bare repo, a newline in a path. This is the
//!   only layer that proves the fixtures the other two assert against are
//!   what git actually emits.
//!
//! The through-line is that every case is an *unhealthy* repository the
//! preflight must reject, not merely a healthy one it must accept: the defect
//! class this module exists to prevent is a preflight that passes while the
//! repository is in the state it was written to catch, and a green run is
//! that defect's symptom rather than its absence.
//!
//! ## Every test here has been observed failing
//!
//! An assertion never seen red is not evidence, which is the whole of issue
//! #568's complaint — so each test below was run against a deliberately broken
//! `repo_state.rs` and confirmed to go red before it was accepted as passing
//! (`.claude/skills/reproduce-first`). Nineteen single-line defects were
//! injected one at a time and every one was caught: dropping
//! `--path-format=absolute`; restoring the pre-#558 argument order; parsing
//! the `--porcelain` fallback with [`RECORD_NUL`]; swallowing the fallback's
//! error into an empty registry; each of the three arms of the `exists`
//! mapping; [`run_git`]'s terminator trim and its non-zero-exit routing;
//! [`should_assert_shallow`] and [`preflight_failures`] stubbed off;
//! [`is_linked_worktree`] pinned to each constant; `paths_degraded` forcing
//! `true`; the shallow flag misread; the [`failures_from`] skip turned into a
//! hard failure; and — a *fixture* defect rather than a code one — a
//! `shallow_clone` helper that omits the `file://` URL, which makes git ignore
//! `--depth` and quietly produce a complete repository for every shallow
//! assertion to pass vacuously against.
//!
//! The two `Sandbox` isolation tests added by issue #834 were held to the
//! same bar, and their subject is the *fixture harness* rather than the code
//! under test — so the injections are into [`Sandbox`] itself. Four, one at a
//! time, every one caught: dropping the [`AMBIENT_LOCATION_VARS`] loop, which
//! reddens the read half (`show-toplevel` reports the ambient `GIT_DIR`'s
//! repository, not the fixture's); dropping that loop *and* reducing the
//! re-exec'd child to building its fixture, which isolates the write half
//! (the victim repository's HEAD moves to a fixture commit); clearing
//! `GIT_CEILING_DIRECTORIES` instead of setting it; and setting it to the
//! sandbox root instead of the root's parent, which is the one that pins why
//! [`Sandbox::ceiling`] returns the parent — the root-listed ceiling escapes
//! to the outer repository exactly as an absent one does.

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
#[derive(Debug)]
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

/// Record terminator for `git worktree list --porcelain -z` (git ≥ 2.36).
/// NUL cannot occur inside a path on any supported platform, so a newline
/// in a worktree path is unambiguous here — which is the whole reason to
/// prefer it.
const RECORD_NUL: u8 = b'\0';

/// Record terminator for plain `--porcelain`, the git < 2.36 fallback.
/// A newline inside a path is indistinguishable from a record break in this
/// form, which is what [`ParsedWorktree::path_maybe_truncated`] exists for.
const RECORD_LF: u8 = b'\n';

/// One `git worktree list --porcelain` entry as parsed, before the
/// filesystem existence check ([`collect`] does that).
#[derive(Debug)]
struct ParsedWorktree {
    path: PathBuf,
    prunable_reason: Option<String>,
    /// True when a line inside this entry's block matched none of the
    /// attributes git emits — the signature of a path containing a literal
    /// newline, which splits the `worktree <path>` line and leaves `path`
    /// truncated. See [`is_known_attribute_line`].
    path_maybe_truncated: bool,
}

/// Whether `line` is one of the attribute lines `git worktree list
/// --porcelain` can emit inside an entry's block, or the blank separator.
///
/// Anything else is the tail of a path that contained a literal newline.
/// The list is closed deliberately: treating an unrecognised line as "some
/// attribute a future git added" would silently restore the fail-green this
/// exists to catch, whereas treating a genuinely new attribute as a
/// truncated path only costs one entry its existence check — the same
/// fail-safe direction the rest of this module takes.
fn is_known_attribute_line(line: &[u8]) -> bool {
    line.is_empty()
        || line == b"bare"
        || line == b"detached"
        || line == b"locked"
        || line == b"prunable"
        || line.starts_with(b"HEAD ")
        || line.starts_with(b"branch ")
        || line.starts_with(b"locked ")
        || line.starts_with(b"prunable ")
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
///
/// `sep` is the record terminator: [`RECORD_NUL`] when [`collect`] got the
/// `-z` output (git ≥ 2.36), where a newline inside a path cannot split a
/// record at all and `path_maybe_truncated` is therefore never set;
/// [`RECORD_LF`] on the older fallback, where it can and is.
fn parse_worktree_entries(porcelain: &[u8], sep: u8) -> Vec<ParsedWorktree> {
    let mut out = Vec::new();
    let mut current: Option<ParsedWorktree> = None;
    for line in porcelain.split(|&b| b == sep) {
        if let Some(rest) = line.strip_prefix(b"worktree ") {
            if let Some(entry) = current.take() {
                out.push(entry);
            }
            current = Some(ParsedWorktree {
                path: PathBuf::from(os_string_from_bytes(rest)),
                prunable_reason: None,
                path_maybe_truncated: false,
            });
        } else if let Some(rest) = line.strip_prefix(b"prunable ")
            && let Some(entry) = current.as_mut()
        {
            entry.prunable_reason = Some(String::from_utf8_lossy(rest).into_owned());
        } else if !is_known_attribute_line(line)
            && let Some(entry) = current.as_mut()
        {
            // A line matching none of the attributes git can emit is the
            // tail of a path that contained a literal newline, so THIS
            // entry's `path` is truncated. Recorded per-entry rather than
            // globally: a sibling entry with an ordinary path is unaffected
            // and must still get a normal existence check, or a genuinely
            // stale worktree goes unreported (Greptile P1 on PR #558,
            // against the first version of this fix, which suppressed drift
            // for every entry at once).
            entry.path_maybe_truncated = true;
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
        // A path field occupied more than one line, so `git_common_dir` and
        // `git_dir` cannot be identified and `is_linked_worktree` is not
        // answerable from this output. Report it as FALSE rather than true.
        //
        // Forcing `true` here looks like the fail-safe choice and is not:
        // it turns the shallow assertion on for a shallow, single-worktree
        // PRIMARY checkout that merely has a newline in its path, which is
        // precisely the exemption issue #557 requires to hold structurally
        // (Greptile P1 on PR #558, against the previous commit). The
        // multi-worktree case does not need this term anyway —
        // `should_assert_shallow` also fires on `worktrees.len() > 1`, and a
        // linked worktree always implies at least two registry entries, so
        // the count carries it. What is genuinely lost is only the
        // belt-and-braces for a checkout that is BOTH linked AND has a
        // registry too drifted to show two entries.
        return Ok(RevParse {
            is_shallow,
            current_worktree,
            is_linked_worktree: false,
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

/// Shells out to git and turns the output into a [`RepoState`]: production's
/// binding of [`collect_with`]'s logic to [`run_git`]'s `Command` spawn, and
/// the only place the two meet.
///
/// Two invocations, kept to that count deliberately (issue #557: "stay
/// cheap"): one combined `rev-parse` for the shallow check, the toplevel
/// and both git-dir flags, plus one `worktree list --porcelain`.
/// `--path-format=absolute` is what makes [`is_linked_worktree`]'s
/// comparison sound rather than incidental — see its doc comment — and the
/// **argument order** is what keeps a newline in a path from disabling the
/// whole preflight; see [`parse_rev_parse`]. Both of those live in
/// [`collect_with`], and `mod fake_git` asserts them there.
fn collect(root: &Path) -> Result<RepoState, String> {
    collect_with(|args| run_git(root, args))
}

/// [`collect`]'s body with the git invocation injected (issue #568).
///
/// The split is what makes the *decisions this function makes about git* —
/// which arguments it asks for and in which order, whether the `-z` probe
/// falls back, and how a `prunable` attribute beats a filesystem check —
/// assertable without a repository in that state being available, or
/// constructible at all. Two of the three properties are invisible from the
/// outside: an argument list has no observable effect on a healthy repo (a
/// dropped `--path-format=absolute` or a reordered `--is-shallow-repository`
/// changes nothing until a subdirectory or a newline path is involved), and
/// the `-z` fallback cannot be triggered at all on the git we run against.
/// Both were fail-greens with no coverage before this seam existed.
///
/// `run` receives exactly the argument vector `collect` would have passed to
/// [`run_git`], so the fake sees the real thing rather than a paraphrase.
/// `collect` remains the only caller in production, and [`run_git`] — the
/// `Command` spawn itself — is now the whole of the untested shell, covered
/// instead by this module's real-git tests.
fn collect_with<F>(run: F) -> Result<RepoState, String>
where
    F: Fn(&[&str]) -> Result<Vec<u8>, String>,
{
    let rev_parse = run(&[
        "rev-parse",
        "--path-format=absolute",
        "--is-shallow-repository",
        "--show-toplevel",
        "--git-common-dir",
        "--git-dir",
    ])?;
    let parsed = parse_rev_parse(&rev_parse)?;
    if parsed.paths_degraded {
        eprintln!(
            "linkage-check: repository-state preflight: a git-reported path contains a newline, \
             so this checkout's linked-worktree status could not be determined; the shallow \
             assertion still applies via the worktree count."
        );
    }

    // `-z` (git ≥ 2.36) terminates each record with NUL, so a newline inside
    // a worktree path cannot split a record and every path arrives exact.
    // That is what makes the truncation handling below dead code on any
    // modern git rather than merely well-tested. Older git rejects the flag,
    // so fall back to the newline form and accept its ambiguity there.
    let (porcelain, sep) = match run(&["worktree", "list", "--porcelain", "-z"]) {
        Ok(out) => (out, RECORD_NUL),
        Err(_) => (run(&["worktree", "list", "--porcelain"])?, RECORD_LF),
    };
    let worktrees = parse_worktree_entries(&porcelain, sep)
        .into_iter()
        .map(|entry| {
            // Unreadable (e.g. EACCES on a live worktree under an unreadable
            // parent) is not evidence of drift — degrade to "present"
            // rather than the false positive `Path::exists()` gives here
            // (it returns `false` on ANY error, permission denied
            // included).
            //
            // A truncated path gets the same treatment for the same reason:
            // a newline in a git-reported path leaves `path` cut short, so
            // `try_exists()` would be asking about a path that was never on
            // disk and would report a live worktree as drifted. git's own
            // `prunable` verdict is unaffected by the mangling — that is
            // why drift keys off it — so where the path text cannot be
            // trusted, trust only what git said.
            //
            // Gated per ENTRY, not on the whole run: an unrelated entry
            // with an ordinary path still gets a real existence check, so a
            // genuinely stale worktree is still reported even while a
            // sibling entry's path is unreadable (Greptile P1 on PR #558).
            let exists = match &entry.prunable_reason {
                Some(_) => false,
                None if entry.path_maybe_truncated => true,
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
        let paths: Vec<PathBuf> = parse_worktree_entries(porcelain.as_bytes(), RECORD_LF)
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
        let paths: Vec<PathBuf> = parse_worktree_entries(porcelain.as_bytes(), RECORD_LF)
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
        assert!(parse_worktree_entries(b"", RECORD_LF).is_empty());
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
        let entries = parse_worktree_entries(porcelain.as_bytes(), RECORD_LF);
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
        let entries = parse_worktree_entries(porcelain.as_bytes(), RECORD_LF);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].path, PathBuf::from("/repo/green/keep"));
        assert_eq!(
            entries[0].prunable_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );
        assert!(
            entries[0].path_maybe_truncated,
            "the `junk` tail line must mark this entry's path untrustworthy"
        );
    }

    /// **Greptile P1 regression test**, filed against the first version of
    /// this fix: suppressing the existence check for the whole run whenever
    /// ANY path was untrustworthy meant a separate, ordinary stale entry
    /// went unreported, so `linkage-check` exited clean without ever
    /// recommending `git worktree prune`. The suppression is now per-entry,
    /// so the newline entry is spared the meaningless check while its
    /// sibling is still tested normally.
    ///
    /// Reachable on any git that omits the `prunable` attribute for a stale
    /// entry — git < 2.36, which predates it, and a **locked** worktree,
    /// which git never marks prunable at any version.
    #[test]
    fn a_truncated_path_does_not_suppress_drift_for_an_unrelated_entry() {
        let porcelain = "worktree /repo/newline/keep\n\
                          junk\n\
                          HEAD 1fb69e17f2d79d384969cdb648969ab3459524da\n\
                          branch refs/heads/sneaky\n\
                          \n\
                          worktree /repo/ordinary-stale\n\
                          HEAD 4e402c166433f702351f18e6e26c51205e19df1e\n\
                          branch refs/heads/stale\n";
        let entries = parse_worktree_entries(porcelain.as_bytes(), RECORD_LF);
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries[0].path_maybe_truncated);
        assert!(
            !entries[1].path_maybe_truncated,
            "an ordinary sibling entry must keep its real existence check"
        );
        assert_eq!(entries[1].prunable_reason, None);
    }

    /// **The structural fix for the newline class.** Byte-for-byte the
    /// record layout real `git worktree list --porcelain -z` produces on
    /// git 2.55.0 for a worktree whose path contains a literal newline:
    /// records are NUL-terminated, so the newline sits harmlessly INSIDE
    /// the `worktree ` record instead of splitting it. The path arrives
    /// exact, nothing is truncated, and `path_maybe_truncated` — with it,
    /// the whole "which entry can I not verify" question that produced two
    /// Greptile P1s — is unreachable on any git ≥ 2.36.
    #[test]
    fn nul_terminated_records_keep_a_newline_path_intact() {
        let porcelain = b"worktree /repo/wt\nnewline\0\
                          HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d\0\
                          branch refs/heads/sneaky\0\0\
                          worktree /repo/ordinary\0\
                          HEAD 4e402c166433f702351f18e6e26c51205e19df1e\0\0"
            as &[u8];
        let entries = parse_worktree_entries(porcelain, RECORD_NUL);
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].path, PathBuf::from("/repo/wt\nnewline"));
        assert!(
            !entries[0].path_maybe_truncated,
            "NUL records cannot truncate a path"
        );
        assert_eq!(entries[1].path, PathBuf::from("/repo/ordinary"));
        assert!(!entries[1].path_maybe_truncated);
    }

    /// The same input read with the OLD newline terminator, to show what
    /// `-z` actually buys: the path is truncated and the entry is flagged
    /// unverifiable. This is the git < 2.36 fallback's behaviour, kept
    /// honest rather than pretended away.
    #[test]
    fn the_lf_fallback_still_truncates_the_same_input() {
        let porcelain = b"worktree /repo/wt\nnewline\n\
                          HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d\n"
            as &[u8];
        let entries = parse_worktree_entries(porcelain, RECORD_LF);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].path, PathBuf::from("/repo/wt"));
        assert!(entries[0].path_maybe_truncated);
    }

    /// Every attribute line git can emit inside an entry block must be
    /// recognised, or it would be mistaken for the tail of a newline-split
    /// path and cost that entry its existence check.
    #[test]
    fn known_attribute_lines_are_not_mistaken_for_a_truncated_path() {
        for line in [
            &b""[..],
            b"bare",
            b"detached",
            b"locked",
            b"locked because I am testing",
            b"prunable",
            b"prunable gitdir file points to non-existent location",
            b"HEAD 9c131d5455485369f6b6b7c9ac8cdd9d5482241d",
            b"branch refs/heads/main",
        ] {
            assert!(
                is_known_attribute_line(line),
                "{:?} must be recognised",
                String::from_utf8_lossy(line)
            );
        }
        assert!(!is_known_attribute_line(b"tail-of-a-split-path"));
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
    /// into a silent skip of the WHOLE preflight. Now the flag is exact and
    /// nothing returns `Err`; the unanswerable linked-worktree question
    /// reports `false` and leaves the worktree count to carry the
    /// multi-worktree case.
    #[test]
    fn parse_rev_parse_survives_a_newline_inside_a_path() {
        let out = b"false\n/probe/wt\nnewline\n/probe/origin/.git\n\
                    /probe/origin/.git/worktrees/wt-newline" as &[u8];
        let parsed = parse_rev_parse(out).expect("must NOT be an Err — Err means a silent skip");
        assert!(!parsed.is_shallow, "the shallow flag must still be exact");
        assert!(parsed.paths_degraded);
        assert!(
            !parsed.is_linked_worktree,
            "an unanswerable question must not be answered `true` — that fires the shallow \
             assertion on an exempt single-worktree primary checkout"
        );
    }

    /// **Greptile P1 regression test.** A shallow, single-worktree PRIMARY
    /// checkout whose path contains a newline must stay EXEMPT. Forcing
    /// `is_linked_worktree = true` on degradation broke exactly this: it
    /// failed an otherwise-exempt `linkage-check`, which is the CI-breaking
    /// direction issue #557 requires to be impossible by construction.
    #[test]
    fn a_newline_path_does_not_defeat_the_single_worktree_shallow_exemption() {
        let out = b"true\n/probe/wt\nnewline\n/probe/.git\n/probe/.git" as &[u8];
        let parsed = parse_rev_parse(out).expect("parses");
        let s = state(
            parsed.is_shallow,
            vec![entry("/probe/wt", true)],
            "/probe/wt",
            parsed.is_linked_worktree,
        );
        assert!(
            preflight_failures(&s).is_empty(),
            "{:?}",
            preflight_failures(&s)
        );
    }

    /// The same shift in a repository that IS shallow AND has two registry
    /// entries: the reorder means this now reports the shallow failure
    /// rather than skipping silently, and the count — not the unanswerable
    /// `is_linked_worktree` — is what carries it.
    #[test]
    fn a_shallow_repo_with_a_newline_path_still_reports_the_shallow_failure() {
        let out = b"true\n/probe/wt\nnewline\n/probe/origin/.git\n\
                    /probe/origin/.git/worktrees/wt-newline" as &[u8];
        let parsed = parse_rev_parse(out).expect("parses");
        let s = state(
            parsed.is_shallow,
            vec![entry("/probe/origin", true), entry("/probe/wt", true)],
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

/// [`collect_with`] driven by a recording stub, for the decisions it makes
/// that leave **no trace on a healthy repository** (issue #568).
///
/// Two things live here and nowhere else. The **git argument vector** is one:
/// `--path-format=absolute` and the position of `--is-shallow-repository` are
/// both documented as correctness-critical, and undoing either one leaves
/// every pure test in `mod tests` green — they only bite on a subdirectory or
/// a newline path, and `mod real_git` covers those consequences while this
/// module covers the cause. The **`-z` → `--porcelain` fallback** is the
/// other, and it is stronger than that: it is *unreachable* on any git that
/// can run these tests at all, since `-z` arrived in 2.36 and `collect`'s
/// `--path-format` requires 2.31, so every git that survives the `rev-parse`
/// call accepts `-z` too. Without a stub that branch has never executed
/// anywhere.
#[cfg(test)]
mod fake_git {
    use super::*;
    use std::cell::RefCell;
    use tempfile::TempDir;

    /// The exact argument vectors `collect_with` is required to ask for, in
    /// order. Spelled out here rather than shared with the implementation
    /// deliberately: a constant both sides read would move in lockstep with
    /// an accidental edit and assert nothing.
    const REV_PARSE_ARGV: &[&str] = &[
        "rev-parse",
        "--path-format=absolute",
        "--is-shallow-repository",
        "--show-toplevel",
        "--git-common-dir",
        "--git-dir",
    ];
    const WORKTREE_Z_ARGV: &[&str] = &["worktree", "list", "--porcelain", "-z"];
    const WORKTREE_LF_ARGV: &[&str] = &["worktree", "list", "--porcelain"];

    /// Records the argument vector of every git invocation `collect_with`
    /// makes, and answers it from a caller-supplied reply.
    struct Recorder {
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Recorder {
        fn new() -> Self {
            Recorder {
                calls: RefCell::new(Vec::new()),
            }
        }

        /// The vectors recorded here are exactly the ones `collect` hands
        /// [`run_git`] — `collect_with` is the body both go through — so an
        /// assertion on them pins the production invocation rather than a
        /// copy of it.
        fn collect(
            &self,
            reply: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
        ) -> Result<RepoState, String> {
            collect_with(|args| {
                self.calls
                    .borrow_mut()
                    .push(args.iter().map(|a| a.to_string()).collect());
                reply(args)
            })
        }

        fn assert_invocations(&self, expected: &[&[&str]]) {
            let calls = self.calls.borrow();
            let actual: Vec<Vec<&str>> = calls
                .iter()
                .map(|c| c.iter().map(String::as_str).collect())
                .collect();
            let expected: Vec<Vec<&str>> = expected.iter().map(|c| c.to_vec()).collect();
            assert_eq!(actual, expected);
        }
    }

    /// A `rev-parse` answer for a primary checkout rooted at `root`.
    ///
    /// No trailing newline, because [`run_git`] strips it: a fake that left
    /// one on would feed `collect_with` a shape production never sees, and
    /// would silently mask the trim being removed (a trailing `\n` splits
    /// into a fourth, empty path value, which trips `paths.len() > 3` and
    /// forces `paths_degraded` on *every* invocation).
    fn rev_parse_reply(root: &Path, shallow: bool) -> Vec<u8> {
        let root = root.display();
        let shallow = if shallow { "true" } else { "false" };
        format!("{shallow}\n{root}\n{root}/.git\n{root}/.git").into_bytes()
    }

    /// One NUL-terminated `worktree list --porcelain -z` entry, as git ≥ 2.36
    /// emits it: every record NUL-terminated, one extra NUL closing the block.
    fn z_entry(path: &Path, branch: &str, prunable: Option<&str>) -> Vec<u8> {
        let mut out = format!("worktree {}\0", path.display()).into_bytes();
        out.extend_from_slice(format!("HEAD {}\0", "0".repeat(40)).as_bytes());
        out.extend_from_slice(format!("branch refs/heads/{branch}\0").as_bytes());
        if let Some(reason) = prunable {
            out.extend_from_slice(format!("prunable {reason}\0").as_bytes());
        }
        out.push(b'\0');
        out
    }

    /// **The argument list is load-bearing and otherwise invisible.** Both of
    /// the properties `collect` documents at length are no-ops on an ordinary
    /// repository: drop `--path-format=absolute` and nothing changes until
    /// someone runs from a subdirectory; move `--is-shallow-repository` back
    /// to last and nothing changes until a path contains a newline. This is
    /// the assertion that fails the moment either is undone, rather than
    /// waiting for the configuration that makes it hurt.
    ///
    /// It also pins the invocation *count* at two (issue #557: "stay cheap"),
    /// and that `-z` succeeding means no fallback call is made.
    #[test]
    fn the_git_invocations_are_exactly_the_two_documented_ones_in_order() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let rec = Recorder::new();
        rec.collect(|args| match args.first().copied() {
            Some("rev-parse") => Ok(rev_parse_reply(root, false)),
            Some("worktree") => Ok(z_entry(root, "main", None)),
            other => panic!("unexpected git invocation {other:?}"),
        })
        .expect("collects");

        rec.assert_invocations(&[REV_PARSE_ARGV, WORKTREE_Z_ARGV]);
    }

    /// The `-z` → plain `--porcelain` fallback for git < 2.36, which no
    /// machine able to run this suite can reach on its own.
    ///
    /// The payload is not "a second call happened" — it is that the fallback
    /// output is parsed with [`RECORD_LF`]. Reusing [`RECORD_NUL`] there
    /// would swallow a multi-entry listing into ONE record, collapsing a
    /// two-worktree repository to one; that turns off `should_assert_shallow`'s
    /// count term, and this shallow two-worktree clone — the exact damage
    /// shape issue #557 exists for — would report clean.
    #[test]
    fn a_git_that_rejects_z_falls_back_to_newline_records_and_parses_them_as_such() {
        let tmp = TempDir::new().expect("tempdir");
        let main = tmp.path().join("main");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&main).expect("mkdir");
        std::fs::create_dir_all(&other).expect("mkdir");

        let rec = Recorder::new();
        let state = rec
            .collect(|args| match args {
                a if a.first() == Some(&"rev-parse") => Ok(rev_parse_reply(&main, true)),
                a if a.contains(&"-z") => Err("error: unknown switch `z'".to_string()),
                _ => Ok(format!(
                    "worktree {}\nHEAD {h}\nbranch refs/heads/main\n\nworktree {}\nHEAD {h}\nbranch refs/heads/other",
                    main.display(),
                    other.display(),
                    h = "0".repeat(40),
                )
                .into_bytes()),
            })
            .expect("collects");

        rec.assert_invocations(&[REV_PARSE_ARGV, WORKTREE_Z_ARGV, WORKTREE_LF_ARGV]);
        assert_eq!(
            state.worktrees.len(),
            2,
            "the fallback output must be split on newlines, not NULs — one record means the \
             whole listing collapsed into a single entry"
        );
        let failures = preflight_failures(&state);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains(UNSHALLOW_REMEDY), "{}", failures[0]);
    }

    /// When the fallback fails too, the error must PROPAGATE.
    ///
    /// `collect_with`'s `match` discards the `-z` error on purpose — an old
    /// git rejecting a flag is not news — and it is a small edit to discard
    /// the second one the same way and carry on with an empty listing. That
    /// reads to the pure layer as a repository with zero worktrees: no drift
    /// to report, and `should_assert_shallow`'s count term off. git failing
    /// outright would produce a clean preflight.
    #[test]
    fn both_worktree_listings_failing_is_an_error_not_an_empty_registry() {
        let tmp = TempDir::new().expect("tempdir");
        let rec = Recorder::new();
        let collected = rec.collect(|args| match args.first().copied() {
            Some("rev-parse") => Ok(rev_parse_reply(tmp.path(), true)),
            _ => Err("fatal: not a git repository".to_string()),
        });

        assert!(collected.is_err(), "must not degrade to an empty registry");
        rec.assert_invocations(&[REV_PARSE_ARGV, WORKTREE_Z_ARGV, WORKTREE_LF_ARGV]);
    }

    /// A `rev-parse` failure propagates rather than being collected around.
    #[test]
    fn a_rev_parse_failure_stops_collection() {
        let rec = Recorder::new();
        let collected = rec.collect(|_| Err("fatal: detected dubious ownership".to_string()));
        assert!(collected.is_err());
        // And nothing further is asked: no worktree listing after the failure.
        rec.assert_invocations(&[REV_PARSE_ARGV]);
    }

    /// git's own `prunable` verdict outranks the filesystem check, and the
    /// directory here **exists** — that is the whole point. It is not a
    /// contrived state either: emptying a worktree's checkout removes the
    /// `.git` file pointing back at the admin directory while leaving the
    /// directory itself, and git reports `prunable gitdir file points to
    /// non-existent location` for it (`mod real_git`'s
    /// `a_prunable_worktree_whose_directory_still_exists_is_still_drift`
    /// builds exactly that with real git).
    ///
    /// A `try_exists()`-first mapping calls that registry clean.
    #[test]
    fn a_prunable_entry_is_missing_even_though_its_directory_exists() {
        let tmp = TempDir::new().expect("tempdir");
        let main = tmp.path().join("main");
        let dead = tmp.path().join("dead");
        std::fs::create_dir_all(&main).expect("mkdir");
        std::fs::create_dir_all(&dead).expect("mkdir");
        assert!(dead.try_exists().expect("stat"), "fixture precondition");

        let rec = Recorder::new();
        let state = rec
            .collect(|args| match args.first().copied() {
                Some("rev-parse") => Ok(rev_parse_reply(&main, false)),
                _ => {
                    let mut out = z_entry(&main, "main", None);
                    out.extend_from_slice(&z_entry(
                        &dead,
                        "dead",
                        Some("gitdir file points to non-existent location"),
                    ));
                    Ok(out)
                }
            })
            .expect("collects");

        let failures = preflight_failures(&state);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains(&format!("{dead:?}")),
            "the still-present but dead worktree must be named: {}",
            failures[0]
        );
        assert!(failures[0].contains(PRUNE_REMEDY), "{}", failures[0]);
    }

    /// The other side of the `exists` mapping: a path the parser could not
    /// read intact is reported **present**, so a live worktree is never
    /// accused of drift over a path that was never on disk in that form.
    /// Only reachable on the git < 2.36 fallback, so only reachable with a
    /// stub.
    #[test]
    fn a_truncated_path_is_treated_as_present_rather_than_as_drift() {
        let tmp = TempDir::new().expect("tempdir");
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).expect("mkdir");
        // `try_exists().ok()` rather than `.expect()`: this module is not
        // Unix-gated, and a newline in a path is a name Win32 may refuse to
        // stat at all rather than report absent. Either answer satisfies the
        // precondition — what must not hold is that it is really there.
        let ghost = tmp.path().join("wt\nnewline");
        assert_ne!(
            ghost.try_exists().ok(),
            Some(true),
            "fixture precondition: the untruncated path is not on disk"
        );

        let rec = Recorder::new();
        let state = rec
            .collect(|args| match args {
                a if a.first() == Some(&"rev-parse") => Ok(rev_parse_reply(&main, false)),
                a if a.contains(&"-z") => Err("error: unknown switch `z'".to_string()),
                _ => Ok(format!(
                    "worktree {}\nnewline\nHEAD {}\nbranch refs/heads/sneaky",
                    tmp.path().join("wt").display(),
                    "0".repeat(40),
                )
                .into_bytes()),
            })
            .expect("collects");

        assert_eq!(state.worktrees.len(), 1, "{:?}", state.worktrees);
        assert!(
            state.worktrees[0].exists,
            "an unverifiable path must degrade to present, not to drift"
        );
        assert!(preflight_failures(&state).is_empty());
    }

    /// And the ordinary branch between those two: no `prunable` attribute, a
    /// path that reads fine and simply is not there. This is the case git
    /// < 2.36 leaves entirely to us, and a **locked** worktree leaves to us
    /// at every version — git never marks one prunable.
    #[test]
    fn a_registered_path_that_is_simply_gone_is_drift() {
        let tmp = TempDir::new().expect("tempdir");
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).expect("mkdir");
        let gone = tmp.path().join("gone");

        let rec = Recorder::new();
        let state = rec
            .collect(|args| match args.first().copied() {
                Some("rev-parse") => Ok(rev_parse_reply(&main, false)),
                _ => {
                    let mut out = z_entry(&main, "main", None);
                    out.extend_from_slice(&z_entry(&gone, "gone", None));
                    Ok(out)
                }
            })
            .expect("collects");

        let failures = preflight_failures(&state);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains(&format!("{gone:?}")),
            "{}",
            failures[0]
        );
    }
}

/// [`collect`], [`run_git`] and [`run`] against repositories that real `git`
/// puts into the states this preflight exists to reject (issue #568).
///
/// The other two test modules assert against fixtures. This one builds the
/// repository, so it is the layer that can be wrong in the direction that
/// matters: a hand-written fixture only ever proves the parser agrees with
/// whoever typed it, and every case here is instead a repository genuinely in
/// the shape being asserted about — shallow, drifted, bare, linked, or with a
/// newline in its path. The motivating one is `.git/shallow` living in the
/// common dir, so a single `--depth=1` clone degrades every worktree sharing
/// that object store at once; that is built here rather than described.
///
/// **Everything is an unhealthy repository unless it is labelled a control.**
/// A test that only proves `collect` returns `Ok` on a healthy checkout would
/// stay green through every defect this module is guarding, because a
/// preflight that has silently disabled itself also returns `Ok` on a healthy
/// checkout. The controls are here for the reason `/reproduce-first` gives —
/// to make each failure attributable — not as the point.
///
/// **Fast tier, deliberately.** These shell out to git, which CLAUDE.md
/// rule 5 previously recorded as something no `xtask` test did. They are in
/// `cargo test-fast` anyway because they meet the bar the rule is actually
/// protecting: each builds two or three tiny repositories in a
/// `tempfile::tempdir()` with empty commits, touches neither the network nor
/// the repository it is running inside, and the whole module lands in well
/// under a second across nextest's process-per-test parallelism. Moving them
/// behind `--features e2e` would have put the only coverage of the collector
/// in the tier CLAUDE.md rule 5 tells you not to run per task, which is a
/// slower way of arriving back at no coverage.
///
/// **Unix only.** `cargo xtask linkage-check` is a Linux-CI tool — the module
/// says so where [`os_string_from_bytes`] falls back to lossy UTF-8 off Unix
/// — and two of the fixtures are Unix constructs outright: a `file://` URL
/// spelled from a POSIX path, and a directory whose name contains a literal
/// newline, which Win32 rejects. `build-windows` still type-checks and lints
/// this module, and `build-macos` runs it.
///
/// **What is NOT covered here**, so it is not mistaken for covered: the
/// degenerate drift case, where the *current* checkout's own registry entry
/// is the missing one. It has no real-git construction — git cannot report a
/// toplevel for a directory that is not there, and every way of half-removing
/// a worktree either drops it from `worktree list` entirely or re-resolves
/// the toplevel to the primary checkout. `mod tests`'
/// `current_worktree_missing_gets_its_own_message` covers the branch over a
/// built state, which is what that branch's coverage is.
#[cfg(all(test, unix))]
mod real_git {
    use super::*;
    use std::fs;
    use std::process::{Command, Output};
    use tempfile::TempDir;

    /// `collect` passes `--path-format`, added in git 2.31 (March 2021).
    const MIN_GIT: (u32, u32) = (2, 31);

    /// `git worktree list --porcelain -z` and the `prunable` attribute both
    /// arrived in git 2.36 (April 2022).
    const GIT_WITH_Z_AND_PRUNABLE: (u32, u32) = (2, 36);

    /// The local git's `(major, minor)`, read rather than assumed.
    ///
    /// Only two things below actually change shape with the git version — the
    /// `-z` record layout and the `prunable` attribute, both from 2.36 — and
    /// each is asserted against this explicitly, so a test either states the
    /// version it depends on or does not depend on one. What it must never
    /// become is a silent dependency on whatever this machine happens to ship.
    fn git_version() -> (u32, u32) {
        let out = Command::new("git")
            .arg("--version")
            .output()
            .expect("`git --version` must run: this module drives real git");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // "git version 2.55.0", or "git version 2.39.5 (Apple Git-154)".
        let version = text.split_whitespace().nth(2).unwrap_or_default();
        let mut parts = version.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        assert!(
            (major, minor) != (0, 0),
            "could not read a version out of {text:?}"
        );
        (major, minor)
    }

    /// Asserts rather than skips, on purpose. A skip is how a check quietly
    /// stops running in the places that matter — the defect class this whole
    /// module exists to gate — so an under-version git fails loudly and says
    /// which version it needed and what for.
    fn require_git(min: (u32, u32), what: &str) {
        let have = git_version();
        assert!(
            have >= min,
            "this fixture needs git >= {}.{} ({what}); this machine has {}.{}. \
             Deliberately an assertion and not a skip: silently not running is the \
             failure mode issue #568 is about.",
            min.0,
            min.1,
            have.0,
            have.1
        );
    }

    /// The environment variables through which git's *location* discovery can
    /// be steered from outside the process (issue #834) — the location-side
    /// counterparts of the `GIT_CONFIG_*` pair, and every one of them
    /// outranks the `current_dir` a fixture invocation passes.
    ///
    /// Cleared rather than overridden, because for each of these "unset" *is*
    /// the default git behaviour — measured: with `GIT_DIR` aimed at another
    /// repository, `git -C <fixture> log` reports that repository's history,
    /// and the same command with `GIT_DIR` removed reports the fixture's.
    ///
    /// `GIT_CEILING_DIRECTORIES` is deliberately NOT in this list:
    /// [`Sandbox::git`] *sets* it (see [`Sandbox::ceiling`]) rather than
    /// clearing it, and `GIT_DEFAULT_HASH`/`GIT_DEFAULT_REF_FORMAT` are not
    /// either — they steer the *format* of a newly created repository, not
    /// its location, and are handled alongside the reproducibility claim.
    const AMBIENT_LOCATION_VARS: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        // Not in the issue's list, and needed for the same claim: it lets the
        // upward walk cross a mount point, which is one of the two things
        // that bound it at all.
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ];

    /// A throwaway tree of repositories under a `tempfile::tempdir()`.
    ///
    /// Every *fixture* command runs with the ambient git environment switched
    /// off, in three groups, because the claim below needs all three and used
    /// to rest on only the first (issue #834):
    ///
    /// - **Configuration** — `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` pointed
    ///   at a path that does not exist, `HOME` and `XDG_CONFIG_HOME` inside
    ///   the sandbox, an empty template dir so no system hook is installed,
    ///   and the commit identity supplied by environment rather than config.
    /// - **Location** — every variable in [`AMBIENT_LOCATION_VARS`] cleared,
    ///   plus [`Sandbox::ceiling`] bounding the upward walk at the sandbox
    ///   root. Without this the `current_dir` each fixture command passes is
    ///   merely a suggestion: `GIT_DIR` overrides it outright, so an ambient
    ///   one made these fixtures read *and write* the repository it named.
    /// - **New-repository format** — `GIT_DEFAULT_HASH` and
    ///   `GIT_DEFAULT_REF_FORMAT` cleared, which is what the
    ///   byte-identical claim below actually needs: ambient
    ///   `GIT_DEFAULT_HASH=sha256` builds SHA-256 fixtures on a developer's
    ///   machine against SHA-1 on a bare runner (measured, as is
    ///   `GIT_DEFAULT_REF_FORMAT=reftable`).
    ///
    /// So the repositories are byte-identical on a developer's machine and on
    /// a bare runner, and no fixture command here can read or write any
    /// repository outside this sandbox — including the checkout these tests
    /// are running inside.
    ///
    /// That last sentence is a claim about [`Sandbox::git`] and the commands
    /// routed through it. [`collect`] and [`run`] are deliberately *not*
    /// given that environment: they are called exactly the way `main.rs`
    /// calls them, so what is under test is the production invocation rather
    /// than a specially-configured one.
    struct Sandbox {
        /// Kept only to hold the directory open for the test's lifetime.
        _dir: TempDir,
        /// `TempDir::path()` canonicalised. Load-bearing on macOS, where
        /// `/var` is a symlink to `/private/var` and git reports the resolved
        /// form — an uncanonicalised path would make every comparison against
        /// `--show-toplevel` fail for a reason that has nothing to do with
        /// this module.
        root: PathBuf,
    }

    impl Sandbox {
        fn new() -> Sandbox {
            Sandbox::place(TempDir::new().expect("tempdir"))
        }

        /// A sandbox rooted *inside* `parent` rather than in the system temp
        /// dir — the shape a developer gets when `TMPDIR` points somewhere
        /// under a checkout. Only
        /// `sandbox_git_cannot_discover_a_repository_above_its_root` uses it,
        /// because putting the sandbox inside a repository is the one way to
        /// observe [`Sandbox::ceiling`] doing its job.
        fn new_in(parent: &Path) -> Sandbox {
            Sandbox::place(TempDir::new_in(parent).expect("tempdir inside parent"))
        }

        fn place(dir: TempDir) -> Sandbox {
            require_git(
                MIN_GIT,
                "`git rev-parse --path-format`, which `collect` passes",
            );
            let root = dir.path().canonicalize().expect("canonicalize tempdir");
            fs::create_dir_all(root.join("home")).expect("mkdir home");
            fs::create_dir_all(root.join("empty-template")).expect("mkdir template");
            Sandbox { _dir: dir, root }
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        /// The upward-discovery ceiling handed to every fixture command: the
        /// sandbox root's **parent**, not the root itself.
        ///
        /// `GIT_CEILING_DIRECTORIES` names the directories git must not
        /// `chdir` up *into*, and it never excludes an invocation's own cwd —
        /// so listing the root does NOT bound the walk (measured: with the
        /// root listed, a `rev-parse` from the root still resolved an
        /// ancestor repository), while listing the parent stops it exactly at
        /// the root. Discovery *within* the sandbox is unaffected, since a
        /// walk that starts below the root finds its repository before it ever
        /// reaches the ceiling.
        ///
        /// Set rather than merely cleared, deliberately. Clearing restores
        /// git's default unbounded walk, which leaves "no fixture command can
        /// resolve a repository outside this sandbox" true only *contingently*
        /// — it would then hold because no current fixture command happens to
        /// do upward discovery from a non-repository directory (`git init` and
        /// `git clone` do none), and would break silently the day one does, or
        /// the day `TMPDIR` points inside a checkout. Bounding it makes the
        /// claim structural instead.
        ///
        /// Canonical, because [`Sandbox::root`] is and git resolves symlinks
        /// in these entries before comparing them. The list is `:`-separated,
        /// so a temp path containing a colon would split into two useless
        /// entries — that weakens this bound without affecting anything else
        /// above.
        fn ceiling(&self) -> &Path {
            // A canonicalised `TempDir` always has a parent, and falling back
            // to the root only ever weakens the bound.
            self.root.parent().unwrap_or(&self.root)
        }

        /// Runs a fixture git command and hands back its raw [`Output`],
        /// failure included.
        ///
        /// Split out of [`Sandbox::git`] so a test can drive an invocation
        /// that is *expected to fail*: the ceiling's effect is only observable
        /// on a command that must not find a repository, and the asserting
        /// wrapper cannot express that.
        fn try_git(&self, cwd: &Path, args: &[&str]) -> Output {
            let mut cmd = Command::new("git");
            cmd.args(args)
                .current_dir(cwd)
                // Ambient configuration.
                .env("HOME", self.at("home"))
                .env("XDG_CONFIG_HOME", self.at("home/.config"))
                .env("GIT_CONFIG_GLOBAL", self.at("no-such-gitconfig"))
                .env("GIT_CONFIG_SYSTEM", self.at("no-such-gitconfig"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TEMPLATE_DIR", self.at("empty-template"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_NAME", "linkage-check tests")
                .env("GIT_AUTHOR_EMAIL", "tests@example.invalid")
                .env("GIT_COMMITTER_NAME", "linkage-check tests")
                .env("GIT_COMMITTER_EMAIL", "tests@example.invalid")
                // Ambient new-repository format.
                .env_remove("GIT_DEFAULT_HASH")
                .env_remove("GIT_DEFAULT_REF_FORMAT")
                // Ambient location discovery, which outranks `current_dir`.
                .env("GIT_CEILING_DIRECTORIES", self.ceiling());
            for var in AMBIENT_LOCATION_VARS {
                cmd.env_remove(var);
            }
            cmd.output()
                .unwrap_or_else(|e| panic!("failed to invoke `git {}`: {e}", args.join(" ")))
        }

        /// Runs a fixture git command, and fails the test with git's own
        /// stderr if it does not succeed — a fixture that half-built itself
        /// and then produced a green assertion is the same fail-green in
        /// miniature.
        fn git(&self, cwd: &Path, args: &[&str]) -> String {
            let out = self.try_git(cwd, args);
            assert!(
                out.status.success(),
                "fixture command `git {}` failed in {}: {}",
                args.join(" "),
                cwd.display(),
                String::from_utf8_lossy(&out.stderr).trim(),
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        /// A repository with two commits at `<root>/origin`, so a `--depth=1`
        /// clone of it is genuinely truncated rather than merely complete and
        /// tiny. Empty commits: nothing here reads a tree.
        fn origin(&self) -> PathBuf {
            let origin = self.at("origin");
            fs::create_dir_all(&origin).expect("mkdir origin");
            self.git(&origin, &["init", "-q", "-b", "main"]);
            self.git(&origin, &["commit", "-q", "--allow-empty", "-m", "first"]);
            self.git(&origin, &["commit", "-q", "--allow-empty", "-m", "second"]);
            origin
        }

        /// Clones `origin` to depth 1 and **verifies the result really is
        /// shallow** before handing it back.
        ///
        /// The `file://` URL is load-bearing: for a plain local path git
        /// ignores `--depth` (it hard-links the object store instead) and only
        /// warns about it, so the same call written without the URL yields a
        /// COMPLETE repository — and every shallow assertion downstream would
        /// then pass vacuously, which is precisely the fail-green shape this
        /// module is here to prevent. The check below is what makes that
        /// impossible to reintroduce silently.
        fn shallow_clone(&self, origin: &Path, name: &str) -> PathBuf {
            self.git(
                &self.root,
                &[
                    "clone",
                    "-q",
                    "--depth=1",
                    &format!("file://{}", origin.display()),
                    name,
                ],
            );
            let clone = self.at(name);
            assert_eq!(
                self.git(&clone, &["rev-parse", "--is-shallow-repository"]),
                "true",
                "fixture precondition: `--depth=1` must have produced a shallow clone"
            );
            clone
        }

        /// A complete clone — the control for [`Sandbox::shallow_clone`].
        fn full_clone(&self, origin: &Path, name: &str) -> PathBuf {
            self.git(
                &self.root,
                &["clone", "-q", &format!("file://{}", origin.display()), name],
            );
            let clone = self.at(name);
            assert_eq!(
                self.git(&clone, &["rev-parse", "--is-shallow-repository"]),
                "false",
                "fixture precondition: this control must NOT be shallow"
            );
            clone
        }

        /// Adds a linked worktree of `clone` at `<root>/<name>` and confirms
        /// the registry now lists two entries.
        fn add_worktree(&self, clone: &Path, name: &str, branch: &str) -> PathBuf {
            let wt = self.at(name);
            self.git(
                clone,
                &["worktree", "add", "-q", &wt.to_string_lossy(), "-b", branch],
            );
            assert_eq!(
                self.git(clone, &["worktree", "list", "--porcelain"])
                    .matches("worktree ")
                    .count(),
                2,
                "fixture precondition: the clone must now have two registered worktrees"
            );
            wt
        }
    }

    /// Marker in the child's environment: this process is the re-exec'd half
    /// of `sandbox_git_ignores_ambient_location_env`.
    const AMBIENT_CHILD: &str = "DAD_LINKAGE_CHECK_AMBIENT_GIT_CHILD";

    /// That test's own name, used as the child's libtest filter. A rename
    /// that misses this makes the child match zero tests — which libtest
    /// exits 0 for, so the parent asserts on `1 passed` rather than on the
    /// status alone.
    const AMBIENT_TEST: &str = "sandbox_git_ignores_ambient_location_env";

    /// Scenario: git's location discovery is steerable from the environment —
    /// `GIT_DIR` and friends outrank the `current_dir` a fixture command
    /// passes — so with one of them set in the parent process these fixtures
    /// read and write whatever it names instead of the sandbox (issue #834).
    /// Re-execs this test binary with every such variable aimed at a victim
    /// repository, has the child build a sandbox and assert it operated on
    /// its own fixture, then asserts here that the victim's HEAD and commit
    /// count came through unchanged.
    ///
    /// The re-exec is what keeps this honest: the variables have to be in the
    /// environment *before* the process starts to reproduce the real
    /// condition (mid-`rebase --exec`, a pre-commit hook, `bisect run`), and
    /// setting them in-process would be `unsafe` and would race every other
    /// test sharing the process under a threaded runner.
    #[test]
    fn sandbox_git_ignores_ambient_location_env() {
        if std::env::var_os(AMBIENT_CHILD).is_some() {
            ambient_location_child();
            return;
        }

        // The victim: a repository this test must not touch, built by its own
        // sandbox so it is as disposable as everything else here.
        let host = Sandbox::new();
        let victim = host.origin();
        let before = host.git(&victim, &["rev-parse", "HEAD"]);

        let exe = std::env::current_exe().expect("current_exe: this is a test binary");
        let out = Command::new(&exe)
            .args([AMBIENT_TEST, "--nocapture", "--test-threads=1"])
            .env(AMBIENT_CHILD, "1")
            .env("GIT_DIR", victim.join(".git"))
            .env("GIT_WORK_TREE", &victim)
            .env("GIT_COMMON_DIR", victim.join(".git"))
            .env("GIT_INDEX_FILE", victim.join(".git/index"))
            .env("GIT_OBJECT_DIRECTORY", victim.join(".git/objects"))
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                victim.join(".git/objects"),
            )
            .env("GIT_NAMESPACE", "escape")
            .env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", &victim)
            .output()
            .expect("re-exec this test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            out.status.success(),
            "the sandboxed child must ignore every ambient location variable\n\
             --- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "the child must have run exactly the test named by AMBIENT_TEST \
             ({AMBIENT_TEST}); zero matches exit 0 too, which is the \
             fail-green this asserts away\n{stdout}"
        );

        // The write half. `Sandbox::origin`'s `init`/`commit` sequence in the
        // child ran with `GIT_DIR` naming this repository: unfixed, it moves
        // this HEAD to a fixture commit and replaces the tree.
        assert_eq!(
            host.git(&victim, &["rev-parse", "HEAD"]),
            before,
            "the child's fixture commits reached the victim repository — a \
             sandbox helper wrote into a repository it did not create (#834)"
        );
        assert_eq!(
            host.git(&victim, &["rev-list", "--count", "HEAD"]),
            "2",
            "the victim must still be exactly `Sandbox::origin`'s two commits"
        );
    }

    /// The re-exec'd half of the test above, running with every variable in
    /// [`AMBIENT_LOCATION_VARS`] set in its own environment.
    fn ambient_location_child() {
        // Vacuity guard first: if the parent failed to hand these down, every
        // assertion below passes while proving nothing.
        for var in AMBIENT_LOCATION_VARS {
            assert!(
                std::env::var_os(var).is_some(),
                "the parent must set {var} in this child, or this test proves \
                 nothing at all"
            );
        }
        assert!(
            std::env::var_os("GIT_CEILING_DIRECTORIES").is_some(),
            "the parent must set GIT_CEILING_DIRECTORIES too — `Sandbox::git` \
             overrides rather than clears it, so it is not in the list above"
        );

        let sb = Sandbox::new();
        let origin = sb.origin();

        // The read half: `Sandbox::origin` asserts nothing about *where* it
        // built, so ask git which repository the fixture command resolved.
        assert_eq!(
            Path::new(sb.git(&origin, &["rev-parse", "--show-toplevel"]).as_str()),
            origin.as_path(),
            "a fixture command resolved a repository outside its own sandbox: \
             git's location discovery is still steerable from the environment"
        );
        assert_eq!(
            sb.git(&origin, &["log", "-1", "--format=%s"]),
            "second",
            "HEAD must be this sandbox's own second empty commit"
        );
        assert_eq!(
            sb.git(&origin, &["rev-list", "--count", "HEAD"]),
            "2",
            "two commits, built by this sandbox and nothing else"
        );
    }

    /// Scenario: places a sandbox *inside* another repository's working tree —
    /// what a developer gets when `TMPDIR` points under a checkout — and
    /// asserts a fixture invocation from the sandbox root cannot walk up out
    /// of it, while one from inside a fixture repository still resolves
    /// normally.
    ///
    /// The control in the middle is the load-bearing part: the same probe
    /// without the ceiling *does* resolve the outer repository, so the
    /// assertion cannot pass by the escape being impossible to stage. Without
    /// it this test would stay green on a machine where the sandbox simply
    /// has no repository above it, which is every ordinary machine.
    #[test]
    fn sandbox_git_cannot_discover_a_repository_above_its_root() {
        let host = Sandbox::new();
        let outer = host.origin();
        let sb = Sandbox::new_in(&outer);

        // Control: an unbounded walk from the sandbox root reaches `outer`.
        // Everything the real helper neutralises is neutralised here too,
        // except the ceiling — so an ambient `GIT_DIR` cannot make this
        // control fail for an unrelated reason.
        let mut control = Command::new("git");
        control
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&sb.root)
            .env_remove("GIT_CEILING_DIRECTORIES");
        for var in AMBIENT_LOCATION_VARS {
            control.env_remove(var);
        }
        let escaped = control.output().expect("invoke git");
        assert!(
            escaped.status.success(),
            "control: an unbounded walk from the sandbox root must find the \
             outer repository, or this test stages nothing: {}",
            String::from_utf8_lossy(&escaped.stderr).trim()
        );
        assert_eq!(
            Path::new(String::from_utf8_lossy(&escaped.stdout).trim()),
            outer.as_path(),
            "control: the unbounded walk must land on the outer repository"
        );

        // The bounded walk stops at the sandbox root instead.
        let bounded = sb.try_git(&sb.root, &["rev-parse", "--show-toplevel"]);
        assert!(
            !bounded.status.success(),
            "a fixture invocation from the sandbox root resolved a repository \
             above it: {}",
            String::from_utf8_lossy(&bounded.stdout).trim()
        );

        // And the ceiling must not have broken the walk *inside* the sandbox,
        // which is what a root-listed ceiling would have done.
        let fixture = sb.origin();
        let sub = fixture.join("sub");
        fs::create_dir_all(&sub).expect("mkdir sub");
        assert_eq!(
            Path::new(sb.git(&sub, &["rev-parse", "--show-toplevel"]).as_str()),
            fixture.as_path(),
            "the ceiling must still allow the upward walk within the sandbox"
        );
    }

    /// **The motivating case (issue #557).** `.git/shallow` lives in the
    /// common dir, so the single `--depth=1` clone below has truncated the
    /// object store for the linked worktree too — and nothing about either
    /// checkout looks wrong: `git log` works, `git status` is clean, refs
    /// resolve. The preflight must report it from BOTH, because both are
    /// damaged, and a green run from either is the misdiagnosis this exists
    /// to prevent.
    #[test]
    fn a_shallow_clone_with_a_linked_worktree_fails_from_both_checkouts() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.shallow_clone(&origin, "clone");
        let wt = sb.add_worktree(&clone, "wt", "wt");

        for checkout in [&clone, &wt] {
            let failures = run(checkout);
            assert_eq!(
                failures.len(),
                1,
                "from {}: {failures:?}",
                checkout.display()
            );
            assert!(
                failures[0].contains(UNSHALLOW_REMEDY),
                "from {}: {}",
                checkout.display(),
                failures[0]
            );
        }
    }

    /// **Control** — the `actions/checkout@v7` shape, which clones at depth 1
    /// with exactly one worktree. It must stay exempt, or every job in the CI
    /// matrix goes red. This is the direction the gate exists to protect, and
    /// it pairs with the test above: together they show the failure there is
    /// attributable to the shared object store, not to shallowness alone.
    #[test]
    fn a_shallow_single_worktree_clone_is_exempt() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.shallow_clone(&origin, "ci-checkout");
        assert!(run(&clone).is_empty(), "{:?}", run(&clone));
    }

    /// **Control** — two worktrees, complete history. Shows the failure in
    /// `a_shallow_clone_with_a_linked_worktree_fails_from_both_checkouts` is
    /// attributable to the truncated object store and not to merely having a
    /// linked worktree.
    #[test]
    fn a_complete_clone_with_a_linked_worktree_passes() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.full_clone(&origin, "clone");
        let wt = sb.add_worktree(&clone, "wt", "wt");

        for checkout in [&clone, &wt] {
            assert!(
                run(checkout).is_empty(),
                "from {}: {:?}",
                checkout.display(),
                run(checkout)
            );
        }
    }

    /// **`--path-format=absolute` is what makes the primary/linked
    /// distinction sound, and this is the only test that can tell.** Measured
    /// on git 2.55.0: run from a subdirectory *without* that flag,
    /// `--git-common-dir` comes back relative (`../../.git`) while `--git-dir`
    /// is printed absolute, the two compare unequal, and a primary checkout
    /// reads as a linked worktree — which switches the shallow assertion on
    /// for the exempt CI shape above and fails every job in the matrix.
    ///
    /// `cargo xtask` sets the working directory to the workspace root today,
    /// so nothing in production currently runs from a subdirectory. That is
    /// the point: the flag's protection is invisible until it is needed, and
    /// dropping it costs nothing observable until the day it costs CI.
    #[test]
    fn a_shallow_primary_checkout_stays_exempt_from_a_subdirectory() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.shallow_clone(&origin, "ci-checkout");
        let deep = clone.join("sub/deep");
        fs::create_dir_all(&deep).expect("mkdir subdir");

        let state = collect(&deep).expect("collects from a subdirectory");
        assert!(
            !state.is_linked_worktree,
            "a primary checkout read from a subdirectory must not report as linked"
        );
        assert_eq!(state.current_worktree, clone);
        assert!(run(&deep).is_empty(), "{:?}", run(&deep));
    }

    /// The other direction of the same comparison: from a subdirectory of a
    /// *linked* worktree it must still read as linked, so the exemption is
    /// not bought by simply answering `false` everywhere.
    #[test]
    fn a_linked_worktree_reads_as_linked_from_its_own_subdirectory() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.full_clone(&origin, "clone");
        let wt = sb.add_worktree(&clone, "wt", "wt");
        let deep = wt.join("sub/deep");
        fs::create_dir_all(&deep).expect("mkdir subdir");

        let state = collect(&deep).expect("collects from a subdirectory");
        assert!(state.is_linked_worktree);
        assert_eq!(state.current_worktree, wt);
    }

    /// Registry drift: a worktree removed with `rm -rf` instead of `git
    /// worktree remove` leaves its registry entry behind, and that entry is
    /// the only trace of it. The message must name the path and the remedy.
    ///
    /// Git version does change what is asserted here, so it is asserted
    /// explicitly: from 2.36 git reports the entry `prunable <reason>` itself
    /// and the reason is folded into the message verbatim; before that the
    /// attribute does not exist and the drift is found by the
    /// `try_exists()` fallback, with no reason to fold in.
    #[test]
    fn a_worktree_removed_without_pruning_is_reported_as_drift() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.full_clone(&origin, "clone");
        let wt = sb.add_worktree(&clone, "wt", "wt");
        fs::remove_dir_all(&wt).expect("remove the worktree without pruning");

        let failures = run(&clone);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains(&wt.to_string_lossy().into_owned()),
            "{}",
            failures[0]
        );
        assert!(failures[0].contains(PRUNE_REMEDY), "{}", failures[0]);
        if git_version() >= GIT_WITH_Z_AND_PRUNABLE {
            // Read the reason out of git rather than typing it (Greptile P2 on
            // PR #583): the porcelain `prunable` text is a translated string
            // too — a literal msgid in the `de` catalog git ships, machine-
            // readable output notwithstanding — so a hard-coded copy would
            // make this depend on the contributor's locale. Capturing it here
            // also asserts something stricter: that whatever git said is
            // folded in verbatim, not merely that some reason was.
            let porcelain = sb.git(&clone, &["worktree", "list", "--porcelain"]);
            let reason = porcelain
                .lines()
                .find_map(|line| line.strip_prefix("prunable "))
                .expect("git >= 2.36 reports a prunable reason for a removed worktree");
            assert!(
                failures[0].contains(reason),
                "git's own prunable reason must be folded in verbatim.\n  git said: {reason}\n  message: {}",
                failures[0]
            );
        }
    }

    /// **The `try_exists()` fail-green.** The worktree's directory is still
    /// on disk here — only its `.git` file is gone, which is what an
    /// interrupted removal or a stray `rm -rf <wt>/*` leaves behind — so a
    /// filesystem-first existence check reports this registry clean while the
    /// worktree is dead. git says `prunable` for it, and that verdict is what
    /// the mapping has to prefer.
    ///
    /// Needs git >= 2.36 by construction: before the `prunable` attribute
    /// existed there was nothing to prefer, and this state was genuinely
    /// undetectable.
    #[test]
    fn a_prunable_worktree_whose_directory_still_exists_is_still_drift() {
        require_git(
            GIT_WITH_Z_AND_PRUNABLE,
            "the `prunable` attribute, the only way to see this state",
        );
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.full_clone(&origin, "clone");
        let wt = sb.add_worktree(&clone, "wt", "wt");
        fs::remove_dir_all(&wt).expect("empty the checkout");
        fs::create_dir_all(&wt).expect("but leave the directory itself");
        assert!(
            wt.try_exists().expect("stat"),
            "fixture precondition: the directory must still be there"
        );

        let failures = run(&clone);
        assert_eq!(
            failures.len(),
            1,
            "a still-present directory must not read as a healthy worktree: {failures:?}"
        );
        assert!(
            failures[0].contains(&wt.to_string_lossy().into_owned()),
            "{}",
            failures[0]
        );
        assert!(failures[0].contains(PRUNE_REMEDY), "{}", failures[0]);
    }

    /// **Greptile P1, against a real repository.** A shallow, single-worktree
    /// PRIMARY checkout whose path contains a literal newline must stay
    /// exempt. git emits path values unescaped, so `rev-parse` here returns
    /// seven lines rather than four and `parse_rev_parse` cannot identify the
    /// two dir flags; answering that unanswerable question `true` failed an
    /// otherwise-exempt `linkage-check`, which is the CI-breaking direction.
    #[test]
    fn a_shallow_primary_checkout_with_a_newline_in_its_path_stays_exempt() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.shallow_clone(&origin, "ci\ncheckout");

        let state = collect(&clone).expect("collects");
        assert!(state.is_shallow, "fixture precondition");
        assert!(
            !state.is_linked_worktree,
            "an unanswerable linked-worktree question must not be answered `true`"
        );
        assert!(run(&clone).is_empty(), "{:?}", run(&clone));
    }

    /// **The Greptile P1 in the fail-GREEN direction, against a real
    /// repository.** A shallow clone with a linked worktree whose path
    /// contains a newline: `rev-parse` returns five lines instead of four
    /// (measured on git 2.55.0, and rebuilt here rather than pasted). With
    /// `--is-shallow-repository` requested last it absorbed that shift, landed
    /// on a path instead of `true`/`false`, and returned `Err` — which
    /// `failures_from` turns into a silent skip of the WHOLE preflight, in a
    /// repository that is genuinely damaged. Asking for it first puts it on
    /// line 0, where no path can displace it.
    #[test]
    fn a_shallow_linked_worktree_with_a_newline_in_its_path_still_fails() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.shallow_clone(&origin, "clone");
        let wt = sb.add_worktree(&clone, "wt\nnewline", "sneaky");

        let state = collect(&wt).expect("must NOT be an Err — an Err here is a silent skip");
        assert!(state.is_shallow, "the shallow flag must survive the shift");

        let failures = run(&wt);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains(UNSHALLOW_REMEDY), "{}", failures[0]);
    }

    /// The `-z` record layout `mod tests` pins by hand, re-derived from the
    /// local git rather than trusted. This is what keeps the hand-built
    /// fixtures from drifting into fiction: the same newline-in-a-path
    /// worktree, read through the production [`run_git`] call and the
    /// production parser, must yield the exact path with nothing truncated.
    #[test]
    fn real_git_z_output_matches_the_record_layout_the_fixtures_assume() {
        require_git(
            GIT_WITH_Z_AND_PRUNABLE,
            "`git worktree list --porcelain -z`",
        );
        let sb = Sandbox::new();
        let origin = sb.origin();
        let clone = sb.full_clone(&origin, "clone");
        let wt = sb.add_worktree(&clone, "wt\nnewline", "sneaky");

        let raw = run_git(&clone, &["worktree", "list", "--porcelain", "-z"])
            .expect("this git supports -z");
        assert!(
            raw.contains(&RECORD_NUL),
            "records must be NUL-terminated, not newline-terminated"
        );

        let entries = parse_worktree_entries(&raw, RECORD_NUL);
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].path, clone);
        assert_eq!(
            entries[1].path, wt,
            "the newline must sit INSIDE the record, leaving the path exact"
        );
        assert!(
            entries.iter().all(|e| !e.path_maybe_truncated),
            "NUL records cannot truncate a path"
        );
    }

    /// [`run_git`] trims the trailing newline git appends, and that trim is
    /// load-bearing rather than cosmetic: `parse_rev_parse` splits on `\n`, so
    /// one surviving terminator adds a fourth, empty path value, trips the
    /// `paths.len() > 3` degradation branch, and forces `is_linked_worktree`
    /// to `false` on **every** invocation — the linked-worktree half of the
    /// shallow gate off, permanently and silently.
    #[test]
    fn run_git_trims_the_terminator_git_appends() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let out = run_git(&origin, &["rev-parse", "HEAD"]).expect("rev-parse HEAD");
        assert_eq!(out.len(), 40, "{:?}", String::from_utf8_lossy(&out));
        assert!(!out.ends_with(b"\n") && !out.ends_with(b"\r"));
    }

    /// A non-zero git exit becomes an `Err` carrying git's own stderr, rather
    /// than being read as empty output — which `parse_rev_parse` would reject
    /// anyway, but `parse_worktree_entries` would happily read as a repository
    /// with no worktrees at all.
    ///
    /// The expected text is **captured from git, not typed in** (Greptile P2
    /// on PR #583). `fatal: Needed a single revision` is a translated string —
    /// verified as a literal msgid in the `de` catalog git ships — so a
    /// contributor whose locale is generated for a translated language would
    /// fail this test, in the tier everyone runs per task, for a reason that
    /// has nothing to do with the property under test. Running the same
    /// command in the same directory and environment `run_git` uses yields
    /// whatever prose this machine actually produces, which makes the
    /// assertion locale-proof and version-proof while checking something
    /// stronger than a substring: that git's message arrives *verbatim*.
    #[test]
    fn run_git_routes_a_non_zero_exit_to_err_with_gits_own_message() {
        let sb = Sandbox::new();
        let origin = sb.origin();
        let args = ["rev-parse", "--verify", "refs/heads/nope"];

        let direct = Command::new("git")
            .args(args)
            .current_dir(&origin)
            .output()
            .expect("git runs");
        assert!(
            !direct.status.success(),
            "fixture precondition: this invocation must fail"
        );
        let stderr = String::from_utf8_lossy(&direct.stderr).trim().to_string();
        assert!(
            !stderr.is_empty(),
            "fixture precondition: git must have said something on stderr"
        );

        let err = run_git(&origin, &args).expect_err("must not succeed");
        assert!(
            err.contains(&stderr),
            "git's own message must be carried through verbatim.\n  git said: {stderr}\n  run_git: {err}"
        );
        assert!(
            err.contains("git rev-parse --verify"),
            "the failing invocation must be named: {err}"
        );
    }

    /// A directory that is not a repository at all: [`collect`] fails and
    /// [`run`] turns that into the documented fail-open skip rather than
    /// crashing the build. The skip is deliberate — see [`failures_from`] —
    /// and it is the one outcome that must stay a *pass*.
    #[test]
    fn a_directory_that_is_not_a_repository_is_a_documented_skip() {
        let sb = Sandbox::new();
        let plain = sb.at("not-a-repo");
        fs::create_dir_all(&plain).expect("mkdir");

        assert!(collect(&plain).is_err());
        assert!(run(&plain).is_empty());
    }

    /// A **bare** repository, which `failures_from`'s doc comment names as one
    /// of the states routed to that same skip: `--show-toplevel` fails
    /// outright there. Asserted against real git rather than left as a claim
    /// in a comment.
    ///
    /// The assertion is on [`run_git`]'s **own** message rather than on git's
    /// prose (Greptile P2 on PR #583): `this operation must be run in a work
    /// tree` is likewise a msgid in git's translation catalogs, so matching it
    /// would make this fast-tier test depend on the contributor's locale.
    /// Naming the invocation is also the sharper check — it pins the failure
    /// to the `rev-parse` call, which is exactly what `failures_from` claims,
    /// where the prose alone could have come from either invocation.
    #[test]
    fn a_bare_repository_is_a_documented_skip() {
        let sb = Sandbox::new();
        let bare = sb.at("bare.git");
        fs::create_dir_all(&bare).expect("mkdir");
        sb.git(&bare, &["init", "-q", "--bare"]);

        let err = collect(&bare).expect_err("a bare repo has no work tree");
        assert!(
            err.starts_with("`git rev-parse "),
            "`--show-toplevel` is what must fail in a bare repo, so the rev-parse \
             invocation is the one that must be named: {err}"
        );
        assert!(run(&bare).is_empty());
    }
}
