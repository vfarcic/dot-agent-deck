//! The worktree ownership marker — the file that lets the deck prove it
//! created a git worktree.
//!
//! Both halves of the marker live here on purpose. `worktree_reclaim` REMOVES
//! directories, and it may do so unattended only when the marker says the deck
//! created the worktree; the writer and the reader must therefore agree, to the
//! byte, on where that file lives. Splitting them across modules is how they
//! drift, and a drift in the "reader looks somewhere the writer never wrote"
//! direction is silent — every deck worktree simply reads as foreign again,
//! which is exactly the state issue #425 fixed.
//!
//! Four properties, each of which is a way to get this wrong:
//!
//! - **The marker lives in the worktree's own git metadata dir**
//!   (`<repo>/.git/worktrees/<name>/`, resolved by running `git rev-parse
//!   --git-dir` INSIDE the worktree), never anywhere in the working tree. A
//!   marker inside the tree makes `git status --porcelain` non-empty forever,
//!   and the reclaim gate keeps every dirty worktree — so an in-tree marker
//!   would make marked worktrees permanently *un*reclaimable, defeating the
//!   feature it exists to enable. Verified directly: writing the marker into
//!   the admin dir leaves `git status --porcelain` empty, and `git worktree
//!   remove` deletes the admin dir (marker included) along with the worktree,
//!   so the marker never outlives what it describes.
//! - **It is written only where the deck genuinely created the worktree.**
//!   [`write_marker`] is called from exactly one place — the success arm of
//!   `issue_dispatch_run::create_worktree`, the only `git worktree add` in
//!   `src/`. It is deliberately NOT written for
//!   `WorktreeCreation::AlreadyClaimed` (the directory was already there, so
//!   another process created it) and never retroactively for a worktree that
//!   already exists: a marker is an ownership CLAIM on a deletion path, so the
//!   dangerous direction is the false positive. Unmarked worktrees stay
//!   foreign and cost one `--yes` confirmation, which is the fail-safe
//!   direction. This is the same rule `cargo xtask clean-e2e-tmp` follows for
//!   temp roots (`docs/develop/e2e-temp-dirs.md`): ownership is proven or
//!   asserted by an operator, never inferred.
//! - **Best-effort, never fatal.** A marker that cannot be written must not
//!   fail worktree creation or the dispatch that needed it — the cost of a
//!   missing marker is one extra confirmation later. A failed write also
//!   removes whatever it left at the marker path, so that "one extra
//!   confirmation" is what the operator actually gets rather than what the
//!   warning merely promises (issue #946).
//! - **Idempotent.** One whole-file write, no append, so a re-created or
//!   re-attached worktree cannot accumulate state.
//!
//! The content records WHO created the worktree ([`Creator`]) rather than a
//! bare "the deck", so a later reader can tell which dispatch or which
//! issue-dispatch fire is responsible. The gate ([`is_marked`]) never parses
//! this document: the content is informational, and making the gate depend on
//! parsing it would turn every future format change into "all existing
//! worktrees became foreign". It asks two things of the file and no more —
//! that it exists, and that it is not empty. The length is not a format
//! check and cannot go stale the way a parse can; it is there only so the
//! zero-byte residue of a torn write does not read as a claim.
//!
//! One thing here is not about the marker: [`main_worktree_of`] (issue #550),
//! which answers "which checkout outlives this worktree". It lives beside
//! [`git_dir_of`] because it is that function's exact sibling — the same
//! `rev-parse` shape, the same byte-exact [`path_from_bytes`] /
//! [`trim_trailing_newline`] handling, the same join of a relative answer, the
//! same fail-closed `None`. Re-deriving that machinery somewhere else is how
//! two readings of git's geometry drift apart, which is the failure this
//! module's first paragraph is already about.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

/// The name of the marker file that proves the deck created a worktree. Lives
/// in the worktree's OWN git metadata dir — see the module docs for why it is
/// there and nowhere in the working tree.
pub const OWNER_MARKER_FILENAME: &str = "dot-agent-deck-owner";

/// Version of the marker DOCUMENT's shape. Independent of
/// `worktree_reclaim::SCHEMA_VERSION` (the `--json` report): this one versions
/// a file on disk that outlives the process that wrote it. Bump on a field
/// removal or a meaning change; additive fields don't need a bump. Nothing
/// reads it yet — the ownership gate never looks inside the document — so it
/// exists to make the first reader's job possible, not to gate anything
/// today.
pub const MARKER_SCHEMA_VERSION: u32 = 1;

/// Build a `PathBuf` from raw bytes read from `git`'s output (a `-z` path
/// field, or a `rev-parse --git-dir` line) without a lossy UTF-8 round-trip.
/// On Unix a path is an arbitrary byte sequence, so this goes straight
/// through `OsStr`; elsewhere (Windows paths are UTF-16, and `git` there
/// emits UTF-8 on the wire) a lossy fallback is the best available.
#[cfg(unix)]
pub(crate) fn path_from_bytes(field: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(field))
}

#[cfg(not(unix))]
pub(crate) fn path_from_bytes(field: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(field).into_owned())
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

/// The worktree's own git metadata dir, as git itself reports it from inside
/// the worktree. `None` when that cannot be resolved at all (not a git
/// worktree, `git` missing, empty answer) — callers turn that into "not ours"
/// on the read side and "could not mark" on the write side, never into a
/// guessed path.
///
/// Resolved byte-exactly and joined against `worktree_path` when git answers
/// relatively (it answers absolutely for a linked worktree, relatively — a
/// bare `.git` — for a main checkout), so the writer and the reader compute
/// the same path for a worktree whose name is not valid UTF-8.
pub fn git_dir_of(worktree_path: &Path) -> Option<PathBuf> {
    rev_parse_path(worktree_path, "--git-dir")
}

/// The environment variables through which git's *location* discovery can be
/// steered from outside this process (issue #834). Every one of them outranks
/// the `current_dir` a command passes — measured in
/// `xtask/linkage-check/src/repo_state.rs`, where an ambient `GIT_DIR` made
/// `git -C <fixture> log` report a different repository's history entirely.
///
/// That matters more here than it does for a fixture, because the answer is
/// interpolated into the prompt an agent is started with: a daemon lazy-spawned
/// from inside a `rebase --exec`, a pre-commit hook or a `bisect run` carries
/// one of these, and without the scrub every pane it starts would be told to
/// write its durable report into whatever repository that variable named,
/// however unrelated to the pane's own cwd. That is the one outcome this
/// module's fail-closed posture exists to prevent, and it is not a failure a
/// consumer could detect — the variable would be set, and confidently wrong.
///
/// Cleared rather than overridden, because for each of these "unset" *is* git's
/// default. The list mirrors that file's `AMBIENT_LOCATION_VARS`, including
/// `GIT_DISCOVERY_ACROSS_FILESYSTEM` for the same reason it gives.
///
/// `GIT_CEILING_DIRECTORIES` is deliberately NOT cleared, for a different
/// reason than that file's: it can only *narrow* the upward walk, so an
/// ambient one can make this return `None` but can never make it return a
/// different repository — the fail-closed direction. Honouring it also leaves
/// an operator's guard against walking a slow network mount in place, on a
/// path that runs at every pane spawn.
const AMBIENT_LOCATION_VARS: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// `git`, to be run from inside `dir`, with the ambient location environment
/// switched off so the answer depends on `dir` and nothing else.
///
/// Every `git` invocation in this module goes through here, which also makes
/// [`git_dir_of`] — and so the ownership gate and the reclaim path that deletes
/// directories behind it — immune to the same ambient override.
pub(crate) fn git_at(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
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
/// Lives here rather than in either test module because both need it and they
/// cannot share a `#[cfg(test)] mod tests` item — and because a second copy is
/// exactly how the neutralization drifts out of step with [`git_at`].
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

/// Run `git rev-parse <flag>` from inside `dir` and return the single path it
/// answers with, or `None` on any failure at all — `git` missing, `dir` gone,
/// a non-zero exit, an empty answer.
///
/// One flag per invocation on purpose. `rev-parse` happily takes several and
/// answers one line each, but splitting that output on `\n` is only safe for
/// paths that contain no newline, and this module went to the trouble of
/// reading git's bytes verbatim ([`path_from_bytes`], [`trim_trailing_newline`])
/// precisely so a path it cannot round-trip through `String` still resolves.
/// Trimming exactly one trailing newline from a single-value answer keeps that
/// property; splitting a multi-value one gives it up. The cost is a process per
/// flag, on paths that run at pane spawn and at reclaim time — never in a loop.
///
/// Joined against `dir` when git answers relatively (it does for `--git-dir`
/// and `--git-common-dir` in a main checkout — a bare `.git` — and for
/// `--git-common-dir` from a subdirectory, where the answer is `../../.git`
/// relative to the *process* cwd). `--show-toplevel` is always absolute, so
/// the join is a no-op there.
fn rev_parse_path(dir: &Path, flag: &str) -> Option<PathBuf> {
    let out = git_at(dir).args(["rev-parse", flag]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = trim_trailing_newline(&out.stdout);
    if raw.is_empty() {
        return None;
    }
    let path = path_from_bytes(raw);
    Some(if path.is_absolute() {
        path
    } else {
        dir.join(path)
    })
}

/// Whether two paths name the same existing directory, compared through
/// [`std::fs::canonicalize`] so `..` segments and symlinks cannot make equal
/// directories compare unequal — `--git-common-dir` answered from a
/// subdirectory is literally `../../.git`, which never matches `--git-dir`'s
/// `/repo/.git` textually.
///
/// Fails closed: a path that cannot be canonicalized (gone, unreadable) is
/// never "the same as" anything, so an unresolvable comparison can only make
/// [`main_worktree_of`] return `None`, never make it return a wrong path.
/// Comparison only — the canonical forms are deliberately discarded rather
/// than returned, so a checkout reached through a symlink keeps the spelling
/// git itself reports.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The root of the **main worktree** of the repository `dir` belongs to — the
/// checkout that outlives every linked worktree cut from it (issue #550).
///
/// A linked worktree is temporary — closing a dispatched unit's tab removes
/// its copy, and `dot-agent-deck worktree reclaim` removes one whose branch has
/// merged — so an agent working in one has nowhere durable to leave a report or
/// an artifact. This
/// answers where that durable place is, once, at spawn, so no task has to
/// carry the path and no role prompt has to embed a `git rev-parse`
/// incantation. The consumers are the two prompts the deck composes itself —
/// [`crate::orchestrator_context`]'s published context and `crate::dispatch`'s
/// single-unit prompt — each of which interpolates the resolved path as a
/// literal, so the agent reads a path rather than resolving anything.
///
/// **Fail-closed, and that is the point.** Every unresolvable case returns
/// `None` rather than a best guess, so a consumer can tell "the deck could not
/// work out where the main checkout is" (variable absent) from "the deck
/// believes it is here" (variable set) — a wrong path is far worse than a
/// missing one, because the agent writes its report into it and nobody
/// notices. `None` covers: `dir` is not in a git repository at all, `git` is
/// not installed, and the two cases below where git's own answer is not
/// enough to locate a main worktree.
///
/// How it decides, and why it is not simply `dirname` of the common dir (which
/// is how the issue proposed it, and is wrong in two of these six layouts):
///
/// | layout | `--git-dir` vs `--git-common-dir` | answer |
/// | --- | --- | --- |
/// | ordinary checkout, from its root | same | `--show-toplevel` |
/// | ordinary checkout, from a subdirectory | same | `--show-toplevel` |
/// | `--separate-git-dir` main checkout | same (both the relocated store) | `--show-toplevel` |
/// | linked worktree, default `<repo>/.git` layout | differ | the directory holding the common dir |
/// | linked worktree of a `--separate-git-dir` repo | differ | `None` — unrecoverable |
/// | bare repository, with or without linked worktrees | either | `None` — no working tree |
///
/// The `--git-dir == --git-common-dir` test is what separates "I am already in
/// the main worktree" from "I am in a linked one". In the first case git
/// itself has the answer: `--show-toplevel`, which is right even from a
/// subdirectory and even when the repository's store was relocated with
/// `--separate-git-dir`, where the common dir's parent is not the checkout at
/// all. In a bare repository the two also match, and `--show-toplevel` fails
/// with "this operation must be run in a work tree" — which is exactly the
/// `None` a bare repository should produce.
///
/// In the second case the main worktree is the directory that *holds* the
/// common dir, because a linked worktree's admin dir always lives at
/// `<common-dir>/worktrees/<name>`. That inference is then **proved rather
/// than trusted**: the candidate's own `--git-dir` has to be the common dir.
/// Two real layouts fail that check and correctly yield `None` — a linked
/// worktree of a repository whose store was relocated (the store's parent is
/// some unrelated directory, and git keeps no back-pointer from the store to
/// the checkout, so the main worktree is genuinely unrecoverable from here),
/// and a linked worktree of a *bare* repository (which has no main worktree to
/// find). Without the proof both would inject a confidently wrong path.
///
/// A submodule's checkout answers "same" and so resolves to the submodule's
/// own root rather than the superproject's. That is deliberate: the submodule
/// checkout is a durable directory, which is what the caller is asking for.
///
/// **What it costs, measured rather than assumed**: three `git rev-parse`
/// processes for a main worktree and four for a linked one. On a 16-core box
/// with every core saturated by concurrent agent work, the four-call path
/// takes 31–39ms; on an idle one it is a third of that. That is spent once per
/// pane spawn, inside [`crate::agent_pty::spawn`], which already blocks its
/// caller on `openpty` plus a real `fork`/`exec` and is followed by an agent
/// CLI that takes seconds to boot — so it is not a hot path, and it is not
/// worth a cache whose staleness would have to be reasoned about every time a
/// worktree moved. One flag per process is deliberate; [`rev_parse_path`] has
/// the reason.
pub fn main_worktree_of(dir: &Path) -> Option<PathBuf> {
    resolve_main_worktree(dir).map(|(_, path)| path)
}

/// The main worktree, but only when `dir` is inside a **linked** one — `None`
/// when `dir` is already in the main worktree, on top of every case
/// [`main_worktree_of`] returns `None` for.
///
/// The distinction is what makes this worth telling an agent about at all. In
/// an ordinary checkout the main worktree *is* the directory the agent is
/// working in, so naming it says nothing and costs prompt text every
/// orchestration pays for. In a linked worktree it names the one place whose
/// contents survive the worktree being removed, which is the whole of issue
/// #550.
pub fn main_worktree_if_linked(dir: &Path) -> Option<PathBuf> {
    match resolve_main_worktree(dir)? {
        (Placement::Linked, path) => Some(path),
        (Placement::Main, _) => None,
    }
}

/// Where `dir` sits relative to the repository's main worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// `dir` is inside the main worktree already (an ordinary checkout, a
    /// subdirectory of one, a relocated-store checkout, a submodule).
    Main,
    /// `dir` is inside a linked worktree — one `git worktree add` created, and
    /// one `git worktree remove` will take away again.
    Linked,
}

/// The shared body of [`main_worktree_of`] and [`main_worktree_if_linked`]:
/// the main worktree plus which side of it `dir` is on.
fn resolve_main_worktree(dir: &Path) -> Option<(Placement, PathBuf)> {
    let git_dir = git_dir_of(dir)?;
    let common = rev_parse_path(dir, "--git-common-dir")?;

    if same_dir(&git_dir, &common) {
        return Some((Placement::Main, rev_parse_path(dir, "--show-toplevel")?));
    }

    let candidate = common.parent()?;
    if !same_dir(&git_dir_of(candidate)?, &common) {
        return None;
    }
    Some((
        Placement::Linked,
        rev_parse_path(candidate, "--show-toplevel")?,
    ))
}

/// Where this worktree's marker file is, or would be. `None` for the same
/// reasons [`git_dir_of`] returns `None`.
pub fn marker_path(worktree_path: &Path) -> Option<PathBuf> {
    Some(git_dir_of(worktree_path)?.join(OWNER_MARKER_FILENAME))
}

/// Whether `path` holds something this module accepts as an ownership claim:
/// a regular file with at least one byte in it. Any error reading it — gone,
/// unreadable, not a regular file — is `false`.
///
/// Not a parse. See [`is_marked`] for why the length is looked at and the
/// content never is.
fn reads_as_claim(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// Whether the deck can prove it created `worktree_path`. A failure to resolve
/// the git metadata dir, a missing marker, and an EMPTY marker are all `false`
/// — unknown origin must never read as ours.
///
/// **Existence plus non-emptiness, and still never a parse** (issue #946).
/// `std::fs::write` is `File::create` + `write_all`, and `File::create`
/// truncates before the first byte lands, so a write that dies in between
/// leaves a zero-byte file where there may have been nothing at all. Reading
/// that as a claim sends a merged, clean worktree to `Verdict::Remove` — the
/// one verdict that deletes a directory with no confirmation — which is also
/// the case [`write_marker_best_effort`]'s warning promises will ask. When the
/// process itself is killed mid-write, no error path in this module runs to
/// clean up after it, so the gate is where that residue gets caught.
///
/// This is NOT atomicity and does not claim to be: a torn write that got some
/// bytes onto disk still reads as a claim. Closing that would take the
/// temp-and-rename dance [`write_marker`] argues against, for a residue far
/// rarer than the zero-byte one (`File::create` truncates unconditionally,
/// while a short write needs the failure to land mid-`write_all` on a body
/// small enough to be one syscall).
///
/// Meaningful for LINKED worktrees, which is all this is ever asked about: a
/// linked worktree resolves to its private `<repo>/.git/worktrees/<name>`,
/// while a MAIN checkout resolves to the shared `.git` it would have to be
/// marked in. That distinction never comes up in practice — the deck only
/// ever creates linked worktrees, and `worktree_reclaim` skips the main
/// working tree (git lists it first and it is never a reclaim candidate).
pub fn is_marked(worktree_path: &Path) -> bool {
    marker_path(worktree_path).is_some_and(|p| reads_as_claim(&p))
}

/// What created a worktree, recorded in the marker so a later reader can name
/// the responsible task rather than only "the deck".
#[derive(Debug, Clone, Serialize)]
pub struct Creator {
    /// Which creation path ran — a fixed, greppable set rather than free text.
    pub kind: &'static str,
    /// What that path was creating the worktree FOR: the dispatch name, or the
    /// issue-dispatch task and issue number.
    pub subject: String,
}

impl Creator {
    /// `dot-agent-deck dispatch <name>` (and the orchestration spawn that
    /// rides on it — one worktree is created and every role shares it).
    pub fn dispatch(name: &str) -> Self {
        Self {
            kind: "dispatch",
            subject: name.to_string(),
        }
    }

    /// The issue-dispatch fire flow: one worktree per issue.
    pub fn issue_dispatch(task: &str, issue: u64) -> Self {
        Self {
            kind: "issue-dispatch",
            subject: format!("{task}#{issue}"),
        }
    }
}

/// The marker file's content. Informational only — see the module docs on why
/// the ownership gate never parses it.
#[derive(Debug, Serialize)]
struct MarkerDocument<'a> {
    schema: u32,
    created_by: &'a str,
    version: &'a str,
    created_at: String,
    pid: u32,
    creator: &'a Creator,
    branch: &'a str,
}

/// Why a marker write failed — and, the half that matters on a path that
/// DELETES directories, what [`is_marked`] would now see at the marker path.
///
/// The two arms exist so [`write_marker_best_effort`]'s warning can be true
/// rather than merely reassuring. It tells the operator to expect a
/// confirmation prompt at reclaim time, and that expectation holds for
/// [`Self::Clear`] and not for [`Self::ClaimRemains`]; a single `String` error
/// could not tell them apart, which is how the promise came to be made in the
/// case that most commonly emits it (issue #946).
#[derive(Debug)]
pub enum MarkerWriteError {
    /// The write failed and nothing [`is_marked`] reads as a claim is left at
    /// the marker path — either nothing was created, or what was created has
    /// been removed. The worktree reads as foreign, so reclaiming it costs one
    /// explicit confirmation.
    Clear(String),
    /// The write failed and the marker path still holds something
    /// [`is_marked`] reads as a claim. The worktree may still reclaim
    /// unattended, so no confirmation is promised.
    ClaimRemains(String),
}

impl MarkerWriteError {
    /// The human-readable cause, for logs.
    pub fn message(&self) -> &str {
        match self {
            Self::Clear(m) | Self::ClaimRemains(m) => m,
        }
    }
}

impl std::fmt::Display for MarkerWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for MarkerWriteError {}

/// Classify a marker-write failure by what the ownership gate would now see at
/// `path`, rather than by which step failed.
///
/// Deliberately a probe and not a deduction. The interesting cases are the
/// ones where the two disagree: a failure that never created a file can still
/// leave a valid marker from an earlier run behind (a re-mark), and a removal
/// that failed can still leave nothing a claim can be read from (the leftover
/// was zero-length). Only the probe is right in both.
fn classify_write_failure(path: &Path, message: String) -> MarkerWriteError {
    if reads_as_claim(path) {
        MarkerWriteError::ClaimRemains(message)
    } else {
        MarkerWriteError::Clear(message)
    }
}

/// Write the ownership marker for a worktree the deck has just created.
///
/// Returns the path written on success; an `Err` carries a message for the
/// caller to log, plus whether anything the ownership gate reads as a claim
/// was left behind. Every caller treats this as best-effort — see
/// [`write_marker_best_effort`], which is what production code uses.
pub fn write_marker(
    worktree_path: &Path,
    branch: &str,
    creator: &Creator,
) -> Result<PathBuf, MarkerWriteError> {
    let path = marker_path(worktree_path).ok_or_else(|| {
        // No path was resolved, so nothing was touched and there is nothing to
        // probe. `Clear` is the honest answer because `is_marked` resolves the
        // dir the same way and fails the same way; it would be wrong only if
        // this failure were transient AND an earlier run had left a marker
        // there.
        MarkerWriteError::Clear(format!(
            "could not resolve the git metadata dir of {} via `git rev-parse --git-dir`",
            worktree_path.display()
        ))
    })?;
    let doc = MarkerDocument {
        schema: MARKER_SCHEMA_VERSION,
        created_by: "dot-agent-deck",
        version: env!("DAD_VERSION"),
        created_at: chrono::Utc::now().to_rfc3339(),
        pid: std::process::id(),
        creator,
        branch,
    };
    let mut body = match serde_json::to_string_pretty(&doc) {
        Ok(b) => b,
        Err(e) => {
            return Err(classify_write_failure(
                &path,
                format!("could not serialize the ownership marker: {e}"),
            ));
        }
    };
    body.push('\n');
    // Whole-file write, never an append: re-marking a re-attached worktree
    // replaces the document rather than accumulating one per creation.
    //
    // Deliberately NOT a write-to-temp-and-rename. The gate never reads the
    // content, so it needs no atomicity to say the true thing; a rename dance
    // would put a second deck-owned file into git's administrative directory
    // that a crash could strand there — a worse outcome than a truncated
    // informational document. What a failed write leaves behind is removed
    // just below instead, which strands nothing.
    let Err(e) = std::fs::write(&path, body) else {
        return Ok(path);
    };
    let mut message = format!("could not write {}: {e}", path.display());
    // `std::fs::write` is `File::create` + `write_all`, and `File::create`
    // truncates before the first byte is written — so a failure in the write
    // half leaves a zero-length or partial file where a moment ago there may
    // have been nothing. That leftover is not inert: it feeds an ownership
    // gate on a deletion path, and `write_marker_best_effort` is about to tell
    // the operator this worktree will need an explicit confirmation. Clear the
    // path so that is true (issue #946).
    if let Err(remove_err) = std::fs::remove_file(&path)
        && remove_err.kind() != std::io::ErrorKind::NotFound
    {
        message.push_str(&format!(
            "; and what the failed write left at {} could not be removed: {remove_err}",
            path.display()
        ));
    }
    Err(classify_write_failure(&path, message))
}

/// [`write_marker`], made best-effort and non-blocking for the async creation
/// path: a failure warns and is dropped, because the cost of a missing marker
/// is one confirmation prompt at reclaim time and the cost of propagating it
/// would be a failed dispatch.
///
/// Runs on the blocking pool — it spawns `git rev-parse` and touches the
/// filesystem — so it cannot stall the daemon's runtime.
pub async fn write_marker_best_effort(worktree_path: &Path, branch: &str, creator: Creator) {
    let worktree = worktree_path.to_path_buf();
    let branch = branch.to_string();
    let result =
        tokio::task::spawn_blocking(move || write_marker(&worktree, &branch, &creator)).await;
    match result {
        Ok(Ok(path)) => tracing::debug!(
            worktree = %worktree_path.display(),
            marker = %path.display(),
            "wrote the worktree ownership marker"
        ),
        Ok(Err(MarkerWriteError::Clear(e))) => tracing::warn!(
            worktree = %worktree_path.display(),
            error = %e,
            "could not write the worktree ownership marker, and nothing is left at the \
             marker path; the worktree will read as foreign at reclaim time and need an \
             explicit confirmation (this does not affect the worktree itself)"
        ),
        Ok(Err(MarkerWriteError::ClaimRemains(e))) => tracing::warn!(
            worktree = %worktree_path.display(),
            error = %e,
            "could not write the worktree ownership marker, and the marker path still holds \
             a file the ownership gate reads as this deck's claim; the worktree may be \
             reclaimed WITHOUT the usual confirmation, so check that path before running \
             `worktree reclaim` (this does not affect the worktree itself)"
        ),
        // Its own arm rather than a `MarkerWriteError`, because a `JoinError`
        // is a panic inside the closure or a cancellation — so unlike the two
        // above, which are classified by probing the marker path, this one
        // cannot say whether anything was written. It must not borrow either
        // arm's promise.
        Err(e) => tracing::warn!(
            worktree = %worktree_path.display(),
            error = %e,
            "the worktree ownership marker task did not complete, so whether the marker was \
             written is unknown; check the worktree's git metadata dir before running \
             `worktree reclaim` (this does not affect the worktree itself)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory `git rev-parse --git-dir` answers for, so [`marker_path`]
    /// resolves and [`is_marked`] measures something. A plain `git init` is
    /// enough here: nothing in this module needs a LINKED worktree, only a
    /// resolvable metadata dir, and the placement rules that DO need one are
    /// covered by the `create_worktree` tests in `src/issue_dispatch_run.rs`.
    fn git_dir_fixture() -> (tempfile::TempDir, PathBuf) {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create the fixture repo dir");
        let out = Command::new("git")
            .current_dir(&repo)
            .args(["init", "--quiet"])
            .output()
            .expect("run git init");
        assert!(
            out.status.success(),
            "fixture precondition: `git init` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (scratch, repo)
    }

    /// The gate must not accept a zero-byte marker. That file is precisely
    /// what a torn `std::fs::write` leaves behind — `File::create` truncates
    /// before `write_all` runs — and on a merged, clean worktree
    /// `Ownership::Ours` is the one verdict that deletes a directory with no
    /// confirmation (issue #946). Measured in both directions on the same
    /// worktree, so emptiness is the only thing that differs.
    #[test]
    fn a_zero_byte_marker_is_not_an_ownership_claim() {
        let (_scratch, repo) = git_dir_fixture();
        assert!(
            !is_marked(&repo),
            "fixture precondition: an unmarked repo must read as foreign"
        );

        let marker = write_marker(&repo, "agent/gate", &Creator::dispatch("gate"))
            .expect("marking a repo with a resolvable git dir must succeed");
        assert!(
            is_marked(&repo),
            "control: an intact marker must read as ours, or the assertion below measures \
             nothing"
        );

        std::fs::write(&marker, b"").expect("truncate the marker to zero bytes");
        assert!(
            marker.is_file(),
            "fixture precondition: the marker must still BE a file — a removed one is the \
             already-covered plain foreign case, not this one"
        );
        assert!(
            !is_marked(&repo),
            "a zero-byte marker proves nothing about who created this worktree, so it must \
             not resolve to ownership"
        );

        std::fs::write(&marker, b"x").expect("give the marker a byte back");
        assert!(
            is_marked(&repo),
            "one byte is enough: the gate reads a length, never the content, so a document \
             whose shape changes later must not stop counting"
        );
    }

    /// A failed write must leave nothing the gate reads as a claim — which is
    /// what [`write_marker_best_effort`]'s warning promises the operator.
    /// `/dev/full` is the deterministic way to fail the WRITE half rather than
    /// the open: the marker path is a symlink to it, so `File::create`
    /// succeeds and the first `write` returns `ENOSPC`, the same shape as the
    /// full-disk case that most commonly emits the warning.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_failed_marker_write_leaves_nothing_that_reads_as_owned() {
        if !Path::new("/dev/full").exists() {
            println!(
                "SKIP: /dev/full is absent, so a failure of the write half (rather than the \
                 open) cannot be staged here"
            );
            return;
        }
        let (_scratch, repo) = git_dir_fixture();
        let marker = marker_path(&repo).expect("the fixture repo must have a resolvable git dir");
        std::os::unix::fs::symlink("/dev/full", &marker).expect("stage the failing write target");

        let err = write_marker(&repo, "agent/enospc", &Creator::dispatch("enospc"))
            .expect_err("a write that lands on /dev/full must fail");
        assert!(
            matches!(err, MarkerWriteError::Clear(_)),
            "the marker path was cleared, so the failure must carry the arm whose warning \
             promises a confirmation prompt; got {err:?}"
        );
        assert!(
            marker.symlink_metadata().is_err(),
            "the failed write's residue must be REMOVED, not merely left unread — {} is \
             still there",
            marker.display()
        );
        assert!(
            !is_marked(&repo),
            "after a failed write the worktree must read as foreign, which is exactly what \
             the operator was warned to expect"
        );
    }

    /// The other direction, and the one a single-`String` error could not
    /// express: when the failure leaves a real claim behind that cannot be
    /// removed, the warning must NOT promise a confirmation prompt. Staged as
    /// a re-mark of an already-marked repo whose marker is read-only inside a
    /// read-only metadata dir, so the open fails (leaving the earlier, valid
    /// document intact) and the removal fails too.
    #[test]
    #[cfg(unix)]
    fn a_failed_write_that_cannot_clear_the_path_reports_the_surviving_claim() {
        use std::os::unix::fs::PermissionsExt;

        let (_scratch, repo) = git_dir_fixture();
        let marker = write_marker(&repo, "agent/locked", &Creator::dispatch("locked"))
            .expect("the first write must succeed");
        let git_dir = marker
            .parent()
            .expect("the marker lives inside the git metadata dir")
            .to_path_buf();

        let dir_perms = std::fs::metadata(&git_dir)
            .expect("git dir metadata")
            .permissions();
        let file_perms = std::fs::metadata(&marker)
            .expect("marker metadata")
            .permissions();
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o444))
            .expect("make the marker read-only");
        std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o555))
            .expect("make the metadata dir read-only");

        // Root ignores both bits, so the staging would silently not hold and
        // the assertions below would be measuring a successful write.
        let staged = std::fs::OpenOptions::new()
            .write(true)
            .open(&marker)
            .is_err();
        let result =
            staged.then(|| write_marker(&repo, "agent/locked", &Creator::dispatch("locked")));

        // Restore before anything can unwind: a read-only metadata dir is not
        // removable, so a panic above this line would leak the whole tempdir.
        std::fs::set_permissions(&git_dir, dir_perms).expect("restore the metadata dir's mode");
        std::fs::set_permissions(&marker, file_perms).expect("restore the marker's mode");

        let Some(result) = result else {
            println!(
                "SKIP: this process can write through a read-only file, so a leftover that \
                 cannot be removed is not stageable here (running as root?)"
            );
            return;
        };
        let err = result.expect_err("re-marking through an unwritable marker must fail");
        assert!(
            matches!(err, MarkerWriteError::ClaimRemains(_)),
            "a claim that survived the failure must be reported as one, so the warning does \
             not promise a confirmation prompt that will not happen; got {err:?}"
        );
        assert!(
            is_marked(&repo),
            "fixture check: the earlier marker really did survive, so `ClaimRemains` is the \
             accurate answer here rather than a pessimistic one"
        );
    }

    /// The classification probes what the GATE would see rather than deducing
    /// it from which step failed, and these are the three cases where those
    /// two answers can differ. An empty leftover still on disk is `Clear`,
    /// because the gate will not read it as a claim; a non-empty one is
    /// `ClaimRemains` even though nothing in this call created it.
    #[test]
    fn a_failure_is_classified_by_what_the_gate_would_see_not_by_what_failed() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let absent = scratch.path().join("absent");
        let empty = scratch.path().join("empty");
        let document = scratch.path().join("document");
        std::fs::write(&empty, b"").expect("stage an empty leftover");
        std::fs::write(&document, b"{}\n").expect("stage a non-empty leftover");

        for (path, label) in [(&absent, "an absent"), (&empty, "an empty")] {
            let err = classify_write_failure(path, "why".to_string());
            assert!(
                matches!(err, MarkerWriteError::Clear(_)),
                "{label} marker path is one the gate reads as foreign, so the failure must \
                 carry the arm that promises a confirmation prompt; got {err:?}"
            );
        }
        let err = classify_write_failure(&document, "why".to_string());
        assert!(
            matches!(err, MarkerWriteError::ClaimRemains(_)),
            "a non-empty file at the marker path IS read as this deck's claim, whoever wrote \
             it, so the warning must not promise a prompt; got {err:?}"
        );
    }

    // ---------------------------------------------------------------------
    // Issue #550 — `main_worktree_of`. The value gets injected into every
    // spawned agent's environment, so a WRONG answer is silently written into
    // by whatever agent trusts it. Each layout below is therefore asserted
    // either to the exact directory or to `None`; there is no third outcome.
    // ---------------------------------------------------------------------

    /// Run a git command in `dir`, asserting it succeeded. Every fixture
    /// command goes through [`fixture_git`], so an ambient `GIT_DIR` cannot
    /// make an `init`, a `commit` or a `worktree add` here target — and move
    /// the HEAD of — the repository these tests are running inside.
    fn git_in(dir: &Path, sandbox: &Path, args: &[&str]) {
        let out = fixture_git(dir, sandbox)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("run git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "fixture precondition: `git {args:?}` in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// An ordinary checkout with one commit, so `git worktree add` has a
    /// commit-ish to branch from.
    fn checkout_with_a_commit(at: &Path, sandbox: &Path) {
        std::fs::create_dir_all(at).expect("create the fixture checkout dir");
        git_in(at, sandbox, &["init", "--quiet"]);
        git_in(
            at,
            sandbox,
            &["commit", "--quiet", "--allow-empty", "-m", "init"],
        );
    }

    /// Compare through `canonicalize`: on macOS the temp root is reached via a
    /// symlink (`/tmp` → `/private/tmp`), and git reports the resolved
    /// spelling while `TempDir` hands back the symlinked one.
    fn canon(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|e| panic!("canonicalize {}: {e}", p.display()))
    }

    /// An ordinary checkout is its own main worktree. Asserted because the
    /// "same" branch is what covers every non-worktree caller, and because
    /// [`main_worktree_if_linked`] filters exactly this case back out.
    #[test]
    fn main_worktree_of_answers_the_checkout_itself_for_an_ordinary_repo() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        checkout_with_a_commit(&repo, scratch.path());

        let got = main_worktree_of(&repo).expect("an ordinary checkout IS a main worktree");
        assert_eq!(canon(&got), canon(&repo));
    }

    /// A pane's cwd is routinely a subdirectory rather than the repo root, and
    /// git answers `--git-common-dir` RELATIVELY from one (`../../.git`) — so
    /// this is the case that would break a naive textual comparison against
    /// `--git-dir`'s absolute answer, and the reason `same_dir` canonicalizes.
    #[test]
    fn main_worktree_of_answers_the_checkout_root_from_a_subdirectory() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        checkout_with_a_commit(&repo, scratch.path());
        let deep = repo.join("a/b/c");
        std::fs::create_dir_all(&deep).expect("create a nested subdirectory");

        let got = main_worktree_of(&deep).expect("a subdirectory resolves like its checkout");
        assert_eq!(
            canon(&got),
            canon(&repo),
            "the ROOT of the checkout, not the subdirectory the pane happens to sit in"
        );
    }

    /// The case the feature exists for: an agent working in a linked worktree
    /// is told where the checkout that outlives it is. The worktree is created
    /// OUTSIDE the repo (a sibling), which is how this repo's own tooling
    /// places them, so a passing assertion cannot be an accident of nesting.
    #[test]
    fn main_worktree_of_answers_the_main_checkout_from_a_linked_worktree() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        checkout_with_a_commit(&repo, scratch.path());
        let linked = scratch.path().join("repo-feature");
        git_in(
            &repo,
            scratch.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                &linked.to_string_lossy(),
            ],
        );

        let got = main_worktree_of(&linked).expect("a linked worktree has a main worktree");
        assert_eq!(
            canon(&got),
            canon(&repo),
            "a linked worktree must resolve to the MAIN checkout, never to itself — \
             resolving to itself is the stall this exists to remove, dressed as a success"
        );
        assert_ne!(canon(&got), canon(&linked));
    }

    /// The linked-only variant exists so the deck can stay quiet when there is
    /// nothing to say. Both directions asserted on one fixture, so they cannot
    /// drift into disagreeing about the same repository.
    #[test]
    fn main_worktree_if_linked_answers_only_from_a_linked_worktree() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        checkout_with_a_commit(&repo, scratch.path());
        let linked = scratch.path().join("repo-feature");
        git_in(
            &repo,
            scratch.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                &linked.to_string_lossy(),
            ],
        );

        assert_eq!(
            main_worktree_if_linked(&linked).as_deref().map(canon),
            Some(canon(&repo)),
            "from a linked worktree it must name the checkout that outlives it"
        );
        assert_eq!(
            main_worktree_if_linked(&repo),
            None,
            "from the main checkout there is nothing to say — naming the directory the \
             agent is already working in is prompt text every orchestration would pay for"
        );
        assert_eq!(
            main_worktree_if_linked(&repo.join("sub")),
            None,
            "a subdirectory of the main checkout is still the main checkout"
        );
    }

    /// A bare repository has no working tree at all, so there is nothing
    /// durable to point at. `--git-dir` and `--git-common-dir` both answer the
    /// bare directory, and its PARENT — what the issue proposed returning — is
    /// some unrelated directory that would be handed to an agent as a place to
    /// write.
    #[test]
    fn main_worktree_of_is_none_for_a_bare_repository() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let bare = scratch.path().join("repo.git");
        std::fs::create_dir_all(&bare).expect("create the fixture bare dir");
        git_in(&bare, scratch.path(), &["init", "--quiet", "--bare"]);

        assert_eq!(
            main_worktree_of(&bare),
            None,
            "a bare repository has no working tree; the parent of its git dir is not one"
        );
    }

    /// A bare repository CAN have linked worktrees, and they take the "differ"
    /// branch — where the candidate is the bare repo's parent directory. The
    /// proof step is the only thing standing between that and a wrong answer.
    #[test]
    fn main_worktree_of_is_none_for_a_linked_worktree_of_a_bare_repository() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let seed = scratch.path().join("seed");
        checkout_with_a_commit(&seed, scratch.path());
        let bare = scratch.path().join("repo.git");
        git_in(
            &seed,
            scratch.path(),
            &["clone", "--quiet", "--bare", ".", &bare.to_string_lossy()],
        );
        let linked = scratch.path().join("bare-worktree");
        git_in(
            &bare,
            scratch.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                &linked.to_string_lossy(),
            ],
        );

        assert_eq!(
            main_worktree_of(&linked),
            None,
            "the bare repo's parent is the fixture scratch dir — a directory an agent would \
             have been told to write its durable report into"
        );
    }

    /// `git init --separate-git-dir` relocates the store, so the common dir's
    /// parent is NOT the checkout. The main-worktree branch still resolves it
    /// correctly because git itself is asked (`--show-toplevel`) rather than
    /// the path being derived.
    #[test]
    fn main_worktree_of_answers_the_checkout_of_a_relocated_store() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let checkout = scratch.path().join("checkout");
        let store = scratch.path().join("store");
        std::fs::create_dir_all(&checkout).expect("create the fixture checkout dir");
        git_in(
            &checkout,
            scratch.path(),
            &[
                "init",
                "--quiet",
                &format!("--separate-git-dir={}", store.display()),
            ],
        );
        git_in(
            &checkout,
            scratch.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "i"],
        );

        let got = main_worktree_of(&checkout).expect("a relocated store still has a checkout");
        assert_eq!(
            canon(&got),
            canon(&checkout),
            "the checkout, not the store and not the store's parent"
        );
    }

    /// The layout with no answer: from a linked worktree of a repository whose
    /// store was relocated, git keeps no back-pointer from the store to the
    /// main checkout, so the main worktree is genuinely unrecoverable. Both
    /// techniques the issue considered return a confidently wrong path here
    /// (`dirname` of the common dir gives the store's parent; `git worktree
    /// list --porcelain` reports the store itself as the first worktree).
    /// Unset is the only honest answer.
    #[test]
    fn main_worktree_of_is_none_for_a_linked_worktree_of_a_relocated_store() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let checkout = scratch.path().join("checkout");
        let store = scratch.path().join("store");
        std::fs::create_dir_all(&checkout).expect("create the fixture checkout dir");
        git_in(
            &checkout,
            scratch.path(),
            &[
                "init",
                "--quiet",
                &format!("--separate-git-dir={}", store.display()),
            ],
        );
        git_in(
            &checkout,
            scratch.path(),
            &["commit", "--quiet", "--allow-empty", "-m", "i"],
        );
        let linked = scratch.path().join("linked");
        git_in(
            &checkout,
            scratch.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                &linked.to_string_lossy(),
            ],
        );

        assert_eq!(
            main_worktree_of(&linked),
            None,
            "the store's parent is the fixture scratch dir, which is not a checkout — \
             answering it would be worse than answering nothing"
        );
    }

    /// The plainest fail-closed case, and the one a consumer relies on to tell
    /// "not a git worktree" from "wrong path": a directory in no repository at
    /// all yields nothing.
    #[test]
    fn main_worktree_of_is_none_outside_any_repository() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let plain = scratch.path().join("plain");
        std::fs::create_dir_all(&plain).expect("create a non-repo dir");

        // Asserted, not assumed. If the temp root itself sat inside a
        // repository (or this process carries an ambient `GIT_DIR`), the case
        // below would not be the one this test claims to cover — fail loudly
        // rather than pass for the wrong reason.
        let discovery = Command::new("git")
            .current_dir(&plain)
            .args(["rev-parse", "--git-dir"])
            .output()
            .expect("run git rev-parse");
        assert!(
            !discovery.status.success(),
            "fixture precondition: {} unexpectedly resolves to a git dir, so this test would \
             not be exercising the no-repository case",
            plain.display()
        );

        assert_eq!(main_worktree_of(&plain), None);
    }

    // --- ambient git location environment (issue #834, PR #1110 review) ---

    /// Covers temporary process-env mutation in this module's tests. nextest
    /// gives each test its own process, so this only serializes the tests that
    /// share one under a plain `cargo test`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set every ambient location variable at `victim` for the duration of
    /// `body`, restoring whatever was there before. `Command` snapshots the
    /// parent's environment when it spawns, so setting them here is enough to
    /// reproduce the condition a daemon inherits mid-`rebase --exec`, in a
    /// pre-commit hook, or under `bisect run` — no re-exec needed.
    fn with_ambient_git_dir_at(victim: &Path, body: impl FnOnce()) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let vars = [
            ("GIT_DIR", victim.join(".git")),
            ("GIT_WORK_TREE", victim.to_path_buf()),
            ("GIT_COMMON_DIR", victim.join(".git")),
        ];
        let prior: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        // SAFETY: serialized by ENV_LOCK; every prior value is restored below.
        unsafe {
            for (k, v) in &vars {
                std::env::set_var(k, v);
            }
        }
        body();
        unsafe {
            for (k, v) in prior {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// The resolver must answer for the directory it was given and nothing
    /// else. An ambient `GIT_DIR` outranks the `current_dir` a command passes
    /// (issue #834 measured exactly that), and here the consequence is not a
    /// confusing test failure but a *wrong path exported into an agent's
    /// environment* — the variable set, and confidently naming a repository
    /// the pane has nothing to do with. That is the one outcome the
    /// fail-closed posture cannot catch, because the answer looks fine.
    #[test]
    fn main_worktree_of_ignores_an_ambient_git_dir() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        checkout_with_a_commit(&repo, scratch.path());
        let linked = scratch.path().join("repo-feature");
        git_in(
            &repo,
            scratch.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                &linked.to_string_lossy(),
            ],
        );
        let victim = scratch.path().join("victim");
        checkout_with_a_commit(&victim, scratch.path());

        with_ambient_git_dir_at(&victim, || {
            let got = main_worktree_of(&linked).expect("the pane's own repo still resolves");
            assert_eq!(
                canon(&got),
                canon(&repo),
                "the answer must come from the directory passed in, never from an \
                 ambient GIT_DIR the daemon happened to inherit"
            );
            assert_ne!(canon(&got), canon(&victim));
        });
    }

    /// The fixtures themselves must not be steerable either. An ambient
    /// `GIT_DIR` turns `init` / `commit` / `worktree add` into writes against
    /// the repository it names — which on a developer's machine is the
    /// checkout these tests are running inside, whose HEAD then moves. Proved
    /// by recording the victim's HEAD, building a whole fixture under an
    /// ambient `GIT_DIR` aimed at it, and requiring the HEAD not to have moved.
    #[test]
    fn a_fixture_built_under_an_ambient_git_dir_does_not_touch_it() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let victim = scratch.path().join("victim");
        checkout_with_a_commit(&victim, scratch.path());
        let head_of = |at: &Path| {
            let out = fixture_git(at, scratch.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("run git rev-parse HEAD");
            assert!(
                out.status.success(),
                "fixture precondition: HEAD must resolve"
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let before = head_of(&victim);

        with_ambient_git_dir_at(&victim, || {
            let repo = scratch.path().join("repo");
            checkout_with_a_commit(&repo, scratch.path());
            let linked = scratch.path().join("repo-feature");
            git_in(
                &repo,
                scratch.path(),
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    "feature",
                    &linked.to_string_lossy(),
                ],
            );
        });

        assert_eq!(
            head_of(&victim),
            before,
            "a fixture command reached the repository named by the ambient GIT_DIR and \
             moved its HEAD — on a contributor's machine that repository is this checkout"
        );
    }

    /// A cwd that no longer exists resolves to nothing rather than to
    /// whatever the resolving process's own cwd happens to be — the daemon
    /// runs in a checkout of this repo often enough that "falls back to mine"
    /// would look convincing and be wrong.
    #[test]
    fn main_worktree_of_is_none_for_a_directory_that_is_gone() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        assert_eq!(
            main_worktree_of(&scratch.path().join("never-created")),
            None
        );
    }
}
