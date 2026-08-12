//! `dot-agent-deck worktree list|reclaim`.
//!
//! Reclaims a git worktree only when three gates all hold: its PR's state is
//! `MERGED` (via `gh`, never git ancestry — squash-merges never enter `main`'s
//! ancestry, so an ancestry check misses genuinely merged branches), the tree
//! is clean (`git status --porcelain` empty — a merged branch's worktree can
//! still hold uncommitted files that were never part of the PR), and the deck
//! can prove it created the worktree (otherwise the tree is reported as
//! reclaimable-pending-confirmation and removed only with `--yes`). The branch
//! is never deleted, only the worktree directory.
//!
//! Fail-closed throughout: an unresolvable PR state (missing `gh`, a spawn or
//! parse error, or more than one PR matching the branch) means keep, never
//! remove — the gate must be satisfied affirmatively, never by absence of
//! evidence. Unknown ownership resolves to foreign, never to ours.
//!
//! No daemon/protocol involvement: this is a CLI verb that shells out to
//! `git` and `gh` directly, synchronously — no `PROTOCOL_VERSION` bump.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

/// Version of the `--json` document shape. Bump on a field removal or a
/// meaning change; additive fields don't need a bump.
pub const SCHEMA_VERSION: u32 = 1;

/// The name of the marker file that proves the deck created a worktree. Lives
/// in the worktree's OWN git metadata dir (`<repo>/.git/worktrees/<name>/`,
/// found via `git rev-parse --git-dir` run inside the worktree) — outside the
/// working tree, so it never makes `git status --porcelain` report the
/// worktree dirty, and it is removed automatically by `git worktree remove`.
/// Writing this marker at worktree-creation time is out of scope for this
/// task; only reading it is implemented here.
pub const OWNER_MARKER_FILENAME: &str = "dot-agent-deck-owner";

/// Resolved PR state for a worktree's branch, or why it could not be
/// resolved. `Unresolvable` and `NoPr` both keep — the distinction is only
/// for the reported reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrState {
    Merged,
    Open,
    ClosedUnmerged,
    NoPr,
    Unresolvable(String),
}

impl PrState {
    fn label(&self) -> &'static str {
        match self {
            PrState::Merged => "merged",
            PrState::Open => "open",
            PrState::ClosedUnmerged => "closed_unmerged",
            PrState::NoPr => "no_pr",
            PrState::Unresolvable(_) => "unresolvable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Ours,
    Foreign,
}

/// The gate's outcome for one worktree. `Keep` and `Ask` both carry a reason;
/// `Ask` additionally means "would be removed, but requires `--yes`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Remove,
    Ask(String),
    Keep(String),
}

impl Verdict {
    fn label(&self) -> &'static str {
        match self {
            Verdict::Remove => "remove",
            Verdict::Ask(_) => "ask",
            Verdict::Keep(_) => "keep",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Verdict::Remove => None,
            Verdict::Ask(r) | Verdict::Keep(r) => Some(r.as_str()),
        }
    }
}

/// The pure decision gate: (PR state, cleanliness, ownership) -> verdict.
///
/// Evaluation order — and which reason wins when more than one condition
/// applies — is deliberate:
///
/// 1. **PR state first.** Anything other than `Merged` keeps, with a reason
///    naming the PR state, regardless of cleanliness or ownership: a not-yet-
///    merged worktree is not a candidate at all.
/// 2. **Cleanliness second.** A merged-but-dirty worktree keeps with a "dirty"
///    reason even when it is deck-owned — dirty content was never part of the
///    PR, so "the code is already merged" does not cover it. This reason wins
///    over ownership: a dirty foreign worktree is reported as dirty, not as
///    foreign, since dirty is the harder blocker (no flag can override it). An
///    *unresolvable* cleanliness probe (the check itself failed, rather than
///    finding anything dirty) also keeps, but with a distinct reason — it
///    must never be reported as "dirty" when nothing was actually found.
/// 3. **Ownership last.** Only once merged-and-clean is established does
///    ownership decide `Remove` (ours) vs `Ask` (foreign) — cleanliness alone
///    proves nothing about ownership, since a freshly created, not-yet-written
///    worktree is clean by definition.
pub fn decide(pr_state: &PrState, clean: &Cleanliness, ownership: Ownership) -> Verdict {
    match pr_state {
        PrState::Merged => match clean {
            Cleanliness::Dirty => Verdict::Keep(
                "dirty: uncommitted or untracked changes are present that were never part of the merged PR"
                    .to_string(),
            ),
            Cleanliness::Unresolvable(reason) => Verdict::Keep(format!(
                "the cleanliness check itself failed ({reason}) — keeping rather than guessing; \
                 nothing was found in the tree"
            )),
            Cleanliness::Clean => match ownership {
                Ownership::Ours => Verdict::Remove,
                Ownership::Foreign => Verdict::Ask(
                    "reclaimable: PR is merged and the tree is clean, but the deck cannot \
                     prove it created this worktree"
                        .to_string(),
                ),
            },
        },
        PrState::NoPr => Verdict::Keep("no pull request found for this branch".to_string()),
        PrState::Open => Verdict::Keep("pull request is still open".to_string()),
        PrState::ClosedUnmerged => {
            Verdict::Keep("pull request was closed without being merged".to_string())
        }
        PrState::Unresolvable(reason) => Verdict::Keep(format!(
            "pull request state could not be resolved ({reason}) — keeping rather than \
             guessing"
        )),
    }
}

/// Serialize a `PathBuf` as its lossy string rendering, so a worktree path
/// containing non-UTF-8 bytes still produces valid JSON instead of failing
/// the whole document — `PathBuf`'s stock `Serialize` errors on those bytes.
fn serialize_path_lossy<S: serde::Serializer>(
    path: &Path,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
}

/// One examined worktree, ready to render as a human row or a JSON entry.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeReport {
    #[serde(serialize_with = "serialize_path_lossy")]
    pub path: PathBuf,
    pub branch: Option<String>,
    pub clean: bool,
    pub owned: bool,
    pub pr_state: String,
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Top-level `--json` document.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeListDocument {
    pub schema_version: u32,
    pub worktrees: Vec<WorktreeReport>,
}

impl WorktreeListDocument {
    pub fn new(worktrees: Vec<WorktreeReport>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            worktrees,
        }
    }
}

struct RawWorktree {
    path: PathBuf,
    branch: Option<String>,
}

/// Build a `PathBuf` from raw bytes read from `git`'s output (a `-z` path
/// field, or a `rev-parse --git-dir` line) without a lossy UTF-8 round-trip.
/// On Unix a path is an arbitrary byte sequence, so this goes straight
/// through `OsStr`; elsewhere (Windows paths are UTF-16, and `git` there
/// emits UTF-8 on the wire) a lossy fallback is the best available.
#[cfg(unix)]
fn path_from_bytes(field: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(field))
}

/// Strip a single trailing `\n` (or `\r\n`) from a `git` command's raw
/// stdout, at the byte level — no UTF-8 round-trip, so the bytes that
/// precede the line ending survive untouched regardless of what they are.
fn trim_trailing_newline(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_suffix(b"\n")
        .map(|b| b.strip_suffix(b"\r").unwrap_or(b))
        .unwrap_or(bytes)
}

#[cfg(not(unix))]
fn path_from_bytes(field: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(field).into_owned())
}

/// Parse `git worktree list --porcelain -z` into path/branch pairs. `-z`
/// NUL-terminates each field instead of newline-terminating it, which is
/// what makes this safe: `--porcelain`'s default text mode C-quotes a path
/// containing a newline, a double quote, or other characters it decides to
/// escape, so treating that quoted, human-oriented text as a literal
/// filesystem path silently misparses (or entirely skips) such a worktree.
/// A path can never contain a NUL byte, so splitting on NUL is unambiguous
/// regardless of what the path itself contains. An empty field marks the end
/// of a worktree record, mirroring the blank-line separator `-z` replaces.
/// The first entry is always the main working tree (git's own documented
/// ordering); callers skip it, since the primary checkout is never a reclaim
/// candidate.
fn parse_worktree_porcelain(bytes: &[u8]) -> Vec<RawWorktree> {
    let mut result = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch: Option<String> = None;
    for field in bytes.split(|&b| b == 0) {
        if field.is_empty() {
            if let Some(path) = cur_path.take() {
                result.push(RawWorktree {
                    path,
                    branch: cur_branch.take(),
                });
            }
            continue;
        }
        if let Some(rest) = field.strip_prefix(b"worktree ") {
            if let Some(path) = cur_path.take() {
                result.push(RawWorktree {
                    path,
                    branch: cur_branch.take(),
                });
            }
            cur_path = Some(path_from_bytes(rest));
        } else if let Some(rest) = field.strip_prefix(b"branch ") {
            let rest = String::from_utf8_lossy(rest);
            cur_branch = Some(
                rest.strip_prefix("refs/heads/")
                    .unwrap_or(&rest)
                    .to_string(),
            );
        }
    }
    if let Some(path) = cur_path.take() {
        result.push(RawWorktree {
            path,
            branch: cur_branch.take(),
        });
    }
    result
}

/// Enumerate linked worktrees (excludes the main working tree) for the repo
/// rooted at or above `repo_dir`.
fn list_linked_worktrees(repo_dir: &Path) -> Result<Vec<RawWorktree>, String> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .map_err(|e| format!("failed to spawn `git worktree list`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`git worktree list` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut all = parse_worktree_porcelain(&out.stdout);
    if !all.is_empty() {
        all.remove(0); // the main working tree
    }
    Ok(all)
}

/// Outcome of probing a worktree's cleanliness. `Unresolvable` is distinct
/// from `Dirty`: both fail closed to `Keep`, but only `Dirty` means the probe
/// actually found uncommitted or untracked content — `Unresolvable` means the
/// probe itself did not run to completion, so a report must never call it
/// "dirty" (nothing was found; the check failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleanliness {
    Clean,
    Dirty,
    Unresolvable(String),
}

fn check_cleanliness(worktree_path: &Path) -> Cleanliness {
    let out = Command::new("git")
        .current_dir(worktree_path)
        .args(["status", "--porcelain"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            if String::from_utf8_lossy(&o.stdout).trim().is_empty() {
                Cleanliness::Clean
            } else {
                Cleanliness::Dirty
            }
        }
        Ok(o) => Cleanliness::Unresolvable(format!(
            "`git status --porcelain` exited with {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => {
            Cleanliness::Unresolvable(format!("failed to spawn `git status --porcelain`: {e}"))
        }
    }
}

/// Whether the deck can prove it created `worktree_path`: the marker file
/// exists in the worktree's own git metadata dir. Any failure to resolve that
/// dir, or a missing marker, resolves to `Foreign` — unknown must never
/// resolve to `Ours`.
fn ownership_of(worktree_path: &Path) -> Ownership {
    let out = Command::new("git")
        .current_dir(worktree_path)
        .args(["rev-parse", "--git-dir"])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Ownership::Foreign,
    };
    let raw = trim_trailing_newline(&out.stdout);
    if raw.is_empty() {
        return Ownership::Foreign;
    }
    let git_dir = path_from_bytes(raw);
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        worktree_path.join(git_dir)
    };
    if git_dir.join(OWNER_MARKER_FILENAME).is_file() {
        Ownership::Ours
    } else {
        Ownership::Foreign
    }
}

/// Derive a `gh --repo owner/name` slug from the worktree's own `origin`
/// remote — never from `gh`'s own inference, which resolves against the
/// upstream repo when run from a checkout of a GitHub fork that has no
/// default repo configured.
///
/// Fails closed to `None` — the caller turns that into `Unresolvable` (keep,
/// never remove) — on any remote misconfiguration: no `origin` remote, a URL
/// that doesn't parse as `owner/name`, or a host other than `github.com`
/// (`gh` only ever talks to GitHub, so a non-GitHub remote must never resolve
/// to a slug `gh` would misinterpret rather than reject).
fn derive_repo_slug(repo_dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_github_owner_repo(&url)
}

/// Parse a GitHub remote URL (HTTPS, `git@` SSH, or `ssh://` SSH) into an
/// `owner/name` slug. Returns `None` for anything else, including a
/// non-GitHub host, a URL with no path, or a path with more than two
/// segments — fail closed rather than guess.
fn parse_github_owner_repo(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// `gh pr list --head <branch> --state all --repo <owner/name> --json
/// state,headRefName` — `--state all` because `gh pr list` defaults to
/// `--state open`, which makes every merged PR invisible; `--repo`, derived
/// from `origin` via [`derive_repo_slug`], because letting `gh` infer it
/// queries the upstream repo from a fork checkout with no default set.
///
/// Matches results on `headRefName` exactly (mitigates "a PR is matched to
/// the wrong branch" from the PRD's risk list) and treats zero matches as
/// `NoPr`, more than one as `Unresolvable` (ambiguous), never guessing.
fn resolve_pr_state(repo_dir: &Path, branch: &str) -> PrState {
    let repo_slug = match derive_repo_slug(repo_dir) {
        Some(slug) => slug,
        None => {
            return PrState::Unresolvable(
                "could not derive --repo from the origin remote (missing, or not a parseable \
                 GitHub URL)"
                    .to_string(),
            );
        }
    };
    let out = Command::new("gh")
        .current_dir(repo_dir)
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--repo",
            &repo_slug,
            "--json",
            "state,headRefName",
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => return PrState::Unresolvable(format!("gh unavailable: {e}")),
    };
    if !out.status.success() {
        return PrState::Unresolvable(format!(
            "gh pr list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let entries: Vec<serde_json::Value> = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return PrState::Unresolvable(format!("could not parse gh output: {e}")),
    };
    let matching: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|v| v.get("headRefName").and_then(|h| h.as_str()) == Some(branch))
        .collect();
    match matching.as_slice() {
        [] => PrState::NoPr,
        [one] => match one.get("state").and_then(|s| s.as_str()) {
            Some("MERGED") => PrState::Merged,
            Some("OPEN") => PrState::Open,
            Some("CLOSED") => PrState::ClosedUnmerged,
            Some(other) => PrState::Unresolvable(format!("unrecognized PR state {other:?}")),
            None => PrState::Unresolvable("PR entry has no `state` field".to_string()),
        },
        _ => PrState::Unresolvable(format!(
            "{} pull requests matched branch {branch:?}",
            matching.len()
        )),
    }
}

/// Examine every linked worktree of the repo rooted at `repo_dir`: resolve PR
/// state, cleanliness, and ownership for each, and decide its verdict. Pure
/// I/O orchestration; [`decide`] is the tested pure core.
///
/// PR state is resolved against each candidate's OWN path (`wt.path`), never
/// `repo_dir` (the caller's cwd), consistently with `check_cleanliness(&wt.path)`
/// and `ownership_of(&wt.path)`: every per-worktree property is read from the
/// worktree it describes. One concrete case this affects: `remote.<name>.url`
/// is a list-accumulating git config variable, and `git remote get-url`
/// (called by [`derive_repo_slug`]) returns only the first value, so a
/// worktree-scoped `origin` set via `extensions.worktreeConfig` never
/// overrides one already defined in the common config — it only matters when
/// the common config defines no `origin` at all. Resolving against `repo_dir`
/// in that situation yields `Unresolvable`, keeping a worktree forever even
/// though it is genuinely merged and clean; `worktree/reclaim/007` covers
/// exactly this. (Resolving against the wrong repo in general would risk
/// matching an unrelated same-named branch's PR, but that is not a reachable
/// scenario via worktree-scoped remotes — the common config's value always
/// wins.)
pub fn examine_worktrees(repo_dir: &Path) -> Result<Vec<WorktreeReport>, String> {
    let raw = list_linked_worktrees(repo_dir)?;
    let mut reports = Vec::with_capacity(raw.len());
    for wt in raw {
        let cleanliness = check_cleanliness(&wt.path);
        let clean = cleanliness == Cleanliness::Clean;
        let owned = ownership_of(&wt.path) == Ownership::Ours;
        let ownership = if owned {
            Ownership::Ours
        } else {
            Ownership::Foreign
        };
        let pr_state = match &wt.branch {
            Some(branch) => resolve_pr_state(&wt.path, branch),
            None => PrState::Unresolvable("worktree has no branch (detached HEAD)".to_string()),
        };
        let verdict = decide(&pr_state, &cleanliness, ownership);
        reports.push(WorktreeReport {
            path: wt.path,
            branch: wt.branch,
            clean,
            owned,
            pr_state: pr_state.label().to_string(),
            reason: verdict.reason().map(str::to_string),
            verdict: verdict.label().to_string(),
        });
    }
    Ok(reports)
}

const DASH: &str = "-";

fn cell(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or(DASH)
}

/// Render the `worktree list` human table: one row per examined worktree,
/// including its verdict and reason so the output is self-explanatory.
pub fn format_list_human(reports: &[WorktreeReport]) -> String {
    if reports.is_empty() {
        return "no worktrees found\n".to_string();
    }
    let mut out = String::new();
    out.push_str("PATH\tBRANCH\tPR\tCLEAN\tOWNED\tVERDICT\tREASON\n");
    for r in reports {
        let path = r.path.to_string_lossy();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            path,
            cell(&r.branch),
            r.pr_state,
            if r.clean { "yes" } else { "no" },
            if r.owned { "yes" } else { "no" },
            r.verdict,
            r.reason.as_deref().unwrap_or(DASH),
        ));
    }
    out
}

/// Physically remove a worktree directory, preserving its branch:
/// `git -C <repo_dir> worktree remove <path>` — deliberately WITHOUT
/// `--force`, since [`examine_worktrees`] already gated on cleanliness; git's
/// own refusal on an unexpectedly dirty tree is a second line of defense
/// rather than something to override.
fn remove_worktree_dir(repo_dir: &Path, worktree_path: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .args(["worktree", "remove"])
        .arg(worktree_path)
        .output()
        .map_err(|e| format!("failed to spawn `git worktree remove`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The full outcome of a `worktree reclaim` run, partitioned by what actually
/// happened to each examined worktree — used to build both the human report
/// and the exit code.
pub struct ReclaimOutcome {
    pub removed: Vec<WorktreeReport>,
    pub pending: Vec<WorktreeReport>,
    pub kept: Vec<WorktreeReport>,
}

/// Run the reclaim gate and act on it: `Remove`-verdict worktrees are removed
/// unconditionally (ownership already proves it's safe); `Ask`-verdict
/// worktrees are removed only when `yes` is true, otherwise added to
/// `pending`; `Keep`-verdict worktrees are left untouched.
pub fn run_reclaim(repo_dir: &Path, yes: bool) -> Result<ReclaimOutcome, String> {
    let reports = examine_worktrees(repo_dir)?;
    let mut removed = Vec::new();
    let mut pending = Vec::new();
    let mut kept = Vec::new();

    for r in reports {
        match r.verdict.as_str() {
            "remove" => match remove_worktree_dir(repo_dir, &r.path) {
                Ok(()) => removed.push(r),
                Err(e) => {
                    let mut r = r;
                    r.reason = Some(format!("removal failed: {e}"));
                    kept.push(r);
                }
            },
            "ask" if yes => match remove_worktree_dir(repo_dir, &r.path) {
                Ok(()) => removed.push(r),
                Err(e) => {
                    let mut r = r;
                    r.reason = Some(format!("removal failed: {e}"));
                    kept.push(r);
                }
            },
            "ask" => pending.push(r),
            _ => kept.push(r),
        }
    }

    Ok(ReclaimOutcome {
        removed,
        pending,
        kept,
    })
}

/// Render the `worktree reclaim` human report. The ask-surface rules: when
/// a pending decision exists it LEADS the output (never
/// discoverable only by reading past a report), names the exact worktree
/// paths (not a count or category), defaults to keep, and ends with the
/// exact `--yes` command that would proceed — one prompt for the whole batch,
/// not one per worktree.
pub fn format_reclaim_human(outcome: &ReclaimOutcome) -> String {
    let mut out = String::new();

    if !outcome.pending.is_empty() {
        out.push_str(&format!(
            "{} worktree(s) reclaimable pending confirmation (kept for now):\n",
            outcome.pending.len()
        ));
        for r in &outcome.pending {
            out.push_str(&format!("  - {}\n", r.path.to_string_lossy()));
        }
        out.push_str("Run `dot-agent-deck worktree reclaim --yes` to remove them.\n\n");
    }

    if !outcome.removed.is_empty() {
        out.push_str("Removed:\n");
        for r in &outcome.removed {
            out.push_str(&format!("  - {}\n", r.path.to_string_lossy()));
        }
    } else {
        out.push_str("Removed: none\n");
    }

    if !outcome.kept.is_empty() {
        out.push_str("Kept:\n");
        for r in &outcome.kept {
            out.push_str(&format!(
                "  - {} ({})\n",
                r.path.to_string_lossy(),
                r.reason.as_deref().unwrap_or("no reason recorded")
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_merged_clean_owned_removes() {
        let v = decide(&PrState::Merged, &Cleanliness::Clean, Ownership::Ours);
        assert_eq!(v, Verdict::Remove);
    }

    #[test]
    fn decide_merged_clean_foreign_asks() {
        let v = decide(&PrState::Merged, &Cleanliness::Clean, Ownership::Foreign);
        assert!(matches!(v, Verdict::Ask(_)));
    }

    #[test]
    fn decide_merged_dirty_keeps_regardless_of_ownership() {
        let owned = decide(&PrState::Merged, &Cleanliness::Dirty, Ownership::Ours);
        let foreign = decide(&PrState::Merged, &Cleanliness::Dirty, Ownership::Foreign);
        assert!(matches!(owned, Verdict::Keep(ref r) if r.contains("dirty")));
        assert!(matches!(foreign, Verdict::Keep(ref r) if r.contains("dirty")));
    }

    #[test]
    fn decide_merged_unresolvable_cleanliness_keeps_without_calling_it_dirty() {
        let v = decide(
            &PrState::Merged,
            &Cleanliness::Unresolvable("spawn failed".to_string()),
            Ownership::Ours,
        );
        assert!(
            matches!(v, Verdict::Keep(ref r) if !r.contains("dirty") && r.contains("spawn failed"))
        );
    }

    #[test]
    fn decide_no_pr_ancestor_keeps() {
        // The destructive false-positive this PRD exists to prevent: an
        // ancestor branch with no PR must never be removed, even clean and
        // owned.
        let v = decide(&PrState::NoPr, &Cleanliness::Clean, Ownership::Ours);
        assert!(matches!(v, Verdict::Keep(_)));
    }

    #[test]
    fn decide_open_and_closed_unmerged_keep() {
        assert!(matches!(
            decide(&PrState::Open, &Cleanliness::Clean, Ownership::Ours),
            Verdict::Keep(_)
        ));
        assert!(matches!(
            decide(
                &PrState::ClosedUnmerged,
                &Cleanliness::Clean,
                Ownership::Ours
            ),
            Verdict::Keep(_)
        ));
    }

    #[test]
    fn decide_unresolvable_keeps_never_removes() {
        let v = decide(
            &PrState::Unresolvable("gh not found".to_string()),
            &Cleanliness::Clean,
            Ownership::Ours,
        );
        assert!(matches!(v, Verdict::Keep(_)));
    }

    #[test]
    fn parse_porcelain_skips_nothing_and_strips_refs_prefix() {
        let text = "worktree /repo\0HEAD abc123\0branch refs/heads/main\0\0worktree /repo/wt-a\0HEAD def456\0branch refs/heads/feat/a\0\0";
        let parsed = parse_worktree_porcelain(text.as_bytes());
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, PathBuf::from("/repo"));
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].path, PathBuf::from("/repo/wt-a"));
        assert_eq!(parsed[1].branch.as_deref(), Some("feat/a"));
    }

    #[test]
    fn parse_porcelain_preserves_a_path_containing_a_literal_newline() {
        // The exact case newline-delimited parsing cannot handle: a path
        // byte sequence containing `\n` would be C-quoted by `--porcelain`'s
        // default text mode and misparsed (or silently split apart) by a
        // reader that treats each newline as a field terminator. With `-z`,
        // only a NUL byte terminates a field, so a literal `\n` inside the
        // path is just more path content.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"worktree /repo\0HEAD abc123\0branch refs/heads/main\0\0");
        bytes.extend_from_slice(
            b"worktree /repo/wt-\n-embedded\0HEAD def456\0branch refs/heads/feat/weird\0\0",
        );
        let parsed = parse_worktree_porcelain(&bytes);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].path, PathBuf::from("/repo/wt-\n-embedded"));
        assert_eq!(parsed[1].branch.as_deref(), Some("feat/weird"));
    }

    #[test]
    fn json_document_carries_schema_version() {
        let reports = vec![WorktreeReport {
            path: PathBuf::from("/repo/wt-a"),
            branch: Some("feat/a".to_string()),
            clean: true,
            owned: true,
            pr_state: "merged".to_string(),
            verdict: "remove".to_string(),
            reason: None,
        }];
        let json = serde_json::to_string(&WorktreeListDocument::new(reports)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(json.contains("wt-a"));
    }
}
