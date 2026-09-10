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
}
