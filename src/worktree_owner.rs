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
//!   missing marker is one extra confirmation later.
//! - **Idempotent.** One whole-file write, no append, so a re-created or
//!   re-attached worktree cannot accumulate state.
//!
//! The content records WHO created the worktree ([`Creator`]) rather than a
//! bare "the deck", so a later reader can tell which dispatch or which
//! issue-dispatch fire is responsible. The gate itself stays an EXISTENCE
//! check ([`is_marked`]) and never parses this document: the content is
//! informational, and making the gate depend on parsing it would turn every
//! future format change into "all existing worktrees became foreign".

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
/// reads it yet — the ownership gate is an existence check — so it exists to
/// make the first reader's job possible, not to gate anything today.
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
    let out = Command::new("git")
        .current_dir(worktree_path)
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = trim_trailing_newline(&out.stdout);
    if raw.is_empty() {
        return None;
    }
    let git_dir = path_from_bytes(raw);
    Some(if git_dir.is_absolute() {
        git_dir
    } else {
        worktree_path.join(git_dir)
    })
}

/// Where this worktree's marker file is, or would be. `None` for the same
/// reasons [`git_dir_of`] returns `None`.
pub fn marker_path(worktree_path: &Path) -> Option<PathBuf> {
    Some(git_dir_of(worktree_path)?.join(OWNER_MARKER_FILENAME))
}

/// Whether the deck can prove it created `worktree_path`. Any failure to
/// resolve the git metadata dir, and any missing marker, is `false` — unknown
/// origin must never read as ours.
///
/// Meaningful for LINKED worktrees, which is all this is ever asked about: a
/// linked worktree resolves to its private `<repo>/.git/worktrees/<name>`,
/// while a MAIN checkout resolves to the shared `.git` it would have to be
/// marked in. That distinction never comes up in practice — the deck only
/// ever creates linked worktrees, and `worktree_reclaim` skips the main
/// working tree (git lists it first and it is never a reclaim candidate).
pub fn is_marked(worktree_path: &Path) -> bool {
    marker_path(worktree_path).is_some_and(|p| p.is_file())
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

/// Write the ownership marker for a worktree the deck has just created.
///
/// Returns the path written on success; an `Err` carries a message for the
/// caller to log. Every caller treats this as best-effort — see
/// [`write_marker_best_effort`], which is what production code uses.
pub fn write_marker(
    worktree_path: &Path,
    branch: &str,
    creator: &Creator,
) -> Result<PathBuf, String> {
    let path = marker_path(worktree_path).ok_or_else(|| {
        format!(
            "could not resolve the git metadata dir of {} via `git rev-parse --git-dir`",
            worktree_path.display()
        )
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
    let mut body = serde_json::to_string_pretty(&doc)
        .map_err(|e| format!("could not serialize the ownership marker: {e}"))?;
    body.push('\n');
    // Whole-file write, never an append: re-marking a re-attached worktree
    // replaces the document rather than accumulating one per creation.
    //
    // Deliberately NOT a write-to-temp-and-rename. The gate reads existence,
    // not content, so a torn write still says the true thing ("we created
    // this"), while a rename dance would put a second deck-owned file into
    // git's administrative directory that a crash could strand there — a
    // worse outcome than a truncated informational document.
    std::fs::write(&path, body).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
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
    let result = tokio::task::spawn_blocking(move || write_marker(&worktree, &branch, &creator))
        .await
        .unwrap_or_else(|e| Err(format!("the marker-writing task did not run: {e}")));
    match result {
        Ok(path) => tracing::debug!(
            worktree = %worktree_path.display(),
            marker = %path.display(),
            "wrote the worktree ownership marker"
        ),
        Err(e) => tracing::warn!(
            worktree = %worktree_path.display(),
            error = %e,
            "could not write the worktree ownership marker; the worktree will read as \
             foreign at reclaim time and need an explicit confirmation (this does not \
             affect the worktree itself)"
        ),
    }
}
