//! The coordinator-context publish, under the conditions the happy path never
//! reaches (PRD #819 M4).
//!
//! PRD #819 names this file's subject as the risk most likely to ship unbuilt:
//! *"a resolve verb that calls the existing loader will pass a manual smoke test
//! and every functional assertion, while remaining unbounded, symlink-following
//! and blocking on the runtime"*. The same is true of the write. Every test here
//! passes trivially against a `create_dir_all` + `std::fs::write` pair in the
//! ordinary case, and every one of them fails against it in the case it covers.
//!
//! Six publish cases, from the PRD's own list, plus the generated-context bound:
//!
//! 1. a symlinked `.dot-agent-deck` directory component
//! 2. a destination symlink
//! 3. a permissive umask, and a permissive parent directory
//! 4. a partial-write failure leaving no observable half-written context
//! 5. stale-path replacement
//! 6. the resulting permission bits
//!
//! **Fast tier, and deliberately not linked against `tests/common/`.** That
//! module pulls the whole PTY harness into another binary and duplicates its
//! executions; `tests/daemon_protocol.rs` established the alternative that
//! satisfies linkage-check rule 8 — `#[path]`-include the self-contained
//! `src/test_temp.rs`, whose `tempdir()` allocates under the harness root the
//! same way `common::harness_tempdir()` does, for one module and two extra
//! executions.

// Every case below is about `open(2)` flags, `mkdir(2)` modes and Unix
// permission bits. `#[cfg]`-ing the file out on Windows rather than weakening
// the assertions is the disposition PRD #819 asks for: the publish's non-Unix
// path is a NARROWER guarantee (no mode bits, a separate symlink lookup), and a
// test that asserted the narrower thing on both would report the wider claim as
// proven.
#![cfg(unix)]

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use dot_agent_deck::orchestrator_context::{
    CONTEXT_DIR_NAME, CONTEXT_FILE_NAME, ContextPublishError, MAX_CONTEXT_BYTES,
    publish_orchestrator_context,
};

// Issue #322 / linkage-check rule 8: the self-contained scratch-dir resolver,
// included by path rather than through `tests/common/`. Same reasoning as
// `tests/daemon_protocol.rs`.
#[path = "../src/test_temp.rs"]
mod test_temp;

/// A canonical scratch project root.
///
/// Canonicalised because the harness temp base can itself sit behind a symlink
/// and every assertion here compares real paths.
fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = test_temp::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canonicalize the project root");
    (dir, root)
}

fn context_dir(project: &Path) -> PathBuf {
    project.join(CONTEXT_DIR_NAME)
}

fn context_file(project: &Path) -> PathBuf {
    context_dir(project).join(CONTEXT_FILE_NAME)
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// Everything in `.dot-agent-deck` other than the context file itself — i.e.
/// leftover temp files.
fn residue(project: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(context_dir(project)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != CONTEXT_FILE_NAME)
        .collect()
}

/// Case 1. A `.dot-agent-deck` that is a symlink is refused, and nothing is
/// written through it.
///
/// This is the case the replaced `create_dir_all` + `std::fs::write` pair got
/// wrong in the most consequential way: `create_dir_all` is satisfied by a
/// symlink to an existing directory, and the write then lands wherever the link
/// points — carrying the task, the repository-supplied prompt template and the
/// role descriptions with it.
#[test]
fn a_symlinked_context_directory_is_refused_and_nothing_is_written_through_it() {
    let (_guard, project) = project();
    let (_elsewhere_guard, elsewhere) = self::project();
    std::os::unix::fs::symlink(&elsewhere, context_dir(&project)).expect("symlink the component");

    let err = publish_orchestrator_context(&project, "leaked?")
        .expect_err("a symlinked .dot-agent-deck must be refused");
    assert!(
        matches!(err, ContextPublishError::ContextDirIsSymlink),
        "expected ContextDirIsSymlink, got {err:?}"
    );
    assert!(
        !elsewhere.join(CONTEXT_FILE_NAME).exists(),
        "nothing may be written through the link, but {} exists",
        elsewhere.join(CONTEXT_FILE_NAME).display()
    );
}

/// Create `.dot-agent-deck` the way the publish itself does — **explicitly**
/// owner-only, not under the ambient umask.
///
/// This is not tidiness. `mkdir` under a umask of `002` — the default on hosts
/// that give each user a private group, including the box this was written on —
/// produces `0o775`, which the publish now **refuses**
/// (`a_group_or_world_writable_context_directory_is_refused`). A test whose
/// subject is symlinks must not depend on the CI host's umask to reach its
/// assertion, so it says the mode it means.
fn create_context_dir_owner_only(project: &Path) {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(context_dir(project))
        .expect("create .dot-agent-deck owner-only");
}

/// Case 2. A destination that is a symlink is **replaced**, not followed.
///
/// `rename(2)` acts on the directory entry, so the link is unlinked and a real
/// file takes its place. `std::fs::write` does the opposite — it opens the link,
/// follows it, and truncates the target — which is how a coordinator context can
/// end up overwriting a file the operator never named.
#[test]
fn a_destination_symlink_is_replaced_rather_than_followed() {
    let (_guard, project) = project();
    let (_target_guard, target_root) = self::project();
    let target = target_root.join("precious.txt");
    std::fs::write(&target, "do not clobber me").expect("seed the link target");

    create_context_dir_owner_only(&project);
    std::os::unix::fs::symlink(&target, context_file(&project)).expect("symlink the destination");

    publish_orchestrator_context(&project, "the new context").expect("publish must succeed");

    assert_eq!(
        std::fs::read_to_string(&target).expect("read the link target"),
        "do not clobber me",
        "the publish must not have written through the destination symlink"
    );
    let published = context_file(&project);
    assert!(
        !std::fs::symlink_metadata(&published)
            .expect("stat the destination")
            .file_type()
            .is_symlink(),
        "the destination must be a real file after the publish, not still a link"
    );
    assert_eq!(
        std::fs::read_to_string(&published).expect("read the published context"),
        "the new context"
    );
}

/// Case 3 and case 6 together: a permissive umask and a permissive parent grant
/// nothing.
///
/// The mode is an argument to `mkdir(2)` and `open(2)`, so it is applied at
/// creation rather than by a later `chmod` — there is no window in which either
/// inode exists wider than owner-only. A umask can only *remove* bits, so
/// `0o700 & !umask` is owner-only or narrower whatever the process umask is; and
/// a parent directory's mode is not consulted for a new inode's mode at all.
///
/// `serial` is not needed and not used: `umask(2)` is per-process and nextest
/// runs one process per test.
#[test]
fn creation_is_owner_only_under_a_permissive_umask_and_a_permissive_parent() {
    let (_guard, project) = project();
    // As permissive as a directory gets, so a mode inherited from the parent
    // would be visible as 0o777 rather than as a plausible 0o755.
    std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o777))
        .expect("widen the project directory");
    // SAFETY: `umask(2)` is process-global and this test's process is its own
    // under nextest. It is restored below so a future in-process harness change
    // does not inherit it.
    let previous = unsafe { libc::umask(0) };

    publish_orchestrator_context(&project, "owner only").expect("publish must succeed");

    // SAFETY: same call, restoring what was read above.
    unsafe { libc::umask(previous) };

    assert_eq!(
        mode_of(&context_dir(&project)),
        0o700,
        "the context directory must be owner-only even with a zero umask and a 0777 parent"
    );
    assert_eq!(
        mode_of(&context_file(&project)),
        0o600,
        "the context file must be owner-only even with a zero umask and a 0777 parent"
    );
}

/// Case 4. A publish that fails part-way leaves the previous context byte-for-
/// byte intact and no half-written file anywhere.
///
/// This is what "a partial write must never be observable as a coordinator
/// context" actually means, and it is stronger than "the destination is not
/// truncated": the temp file the failed attempt created must be gone too, or the
/// next reader of that directory finds a stray artifact.
///
/// The failure is forced by making `.dot-agent-deck` unwritable, which fails the
/// temp-file create — the earliest failure with a previous context already on
/// disk. Running as root would defeat it (root ignores the mode), so the test
/// says so rather than passing vacuously.
#[test]
fn a_failed_publish_leaves_the_previous_context_intact_and_no_residue() {
    if unsafe { libc::geteuid() } == 0 {
        println!("SKIP: running as root, which ignores the directory mode this case relies on");
        return;
    }
    let (_guard, project) = project();
    publish_orchestrator_context(&project, "the first context").expect("the first publish");
    assert!(
        residue(&project).is_empty(),
        "a clean publish leaves nothing"
    );

    std::fs::set_permissions(
        context_dir(&project),
        std::fs::Permissions::from_mode(0o500),
    )
    .expect("make .dot-agent-deck unwritable");

    let err = publish_orchestrator_context(&project, "the second context")
        .expect_err("a publish into an unwritable directory must fail");
    assert!(
        matches!(err, ContextPublishError::TempCreate(_)),
        "expected TempCreate, got {err:?}"
    );

    std::fs::set_permissions(
        context_dir(&project),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("restore .dot-agent-deck");
    assert_eq!(
        std::fs::read_to_string(context_file(&project)).expect("read the context"),
        "the first context",
        "a failed publish must leave the previous context exactly as it was"
    );
    assert!(
        residue(&project).is_empty(),
        "a failed publish must leave no temp file behind, found {:?}",
        residue(&project)
    );
}

/// Case 5. Republishing over an existing context replaces it wholly — no
/// residue of a longer previous version, and no leftover temp file.
///
/// The length asymmetry is the point: a publish that appended, or that wrote
/// without truncating, would leave the tail of the first context readable at the
/// end of the second. An agent reading that gets two briefs and no way to tell
/// which is current.
#[test]
fn republishing_replaces_a_stale_context_wholly() {
    let (_guard, project) = project();
    let long = "STALE".repeat(4096);
    publish_orchestrator_context(&project, &long).expect("the first publish");
    let first_inode = inode_of(&context_file(&project));

    publish_orchestrator_context(&project, "short").expect("the second publish");

    let published = std::fs::read_to_string(context_file(&project)).expect("read the context");
    assert_eq!(
        published, "short",
        "the second publish must replace the first wholly, not overlay it"
    );
    assert_ne!(
        inode_of(&context_file(&project)),
        first_inode,
        "a rename publishes a NEW inode over the old entry, which is what makes a concurrent \
         reader see one whole version or the other rather than a prefix"
    );
    assert_eq!(
        mode_of(&context_file(&project)),
        0o600,
        "the replacement must be owner-only too — the mode is not inherited from what it replaced"
    );
    assert!(
        residue(&project).is_empty(),
        "found {:?}",
        residue(&project)
    );
}

fn inode_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .ino()
}

/// The generated-context bound refuses rather than truncates, and refuses
/// **before** it touches anything.
///
/// Reached directly rather than through the daemon verb on purpose: the verb's
/// two input bounds (a 1 MiB task, a 1 MiB config) already imply a smaller
/// ceiling, so no request that survives them can reach this one. That makes the
/// bound a backstop rather than the operative gate today — see
/// `MAX_CONTEXT_BYTES`'s own doc — and a backstop that nothing exercises is a
/// backstop nobody notices going missing.
#[test]
fn a_context_past_the_bound_is_refused_and_the_previous_one_is_untouched() {
    let (_guard, project) = project();
    publish_orchestrator_context(&project, "the good context").expect("the first publish");

    let oversized = "x".repeat(MAX_CONTEXT_BYTES + 1);
    let err = publish_orchestrator_context(&project, &oversized)
        .expect_err("a context past the bound must be refused");
    assert!(
        matches!(err, ContextPublishError::ContextTooLarge(n) if n == MAX_CONTEXT_BYTES + 1),
        "expected ContextTooLarge, got {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(context_file(&project)).expect("read the context"),
        "the good context",
        "the refusal must not have truncated or replaced the previous context"
    );

    // Exactly at the bound is accepted, so the check is a bound rather than an
    // off-by-one that refuses legitimate input.
    let at_limit = "y".repeat(MAX_CONTEXT_BYTES);
    publish_orchestrator_context(&project, &at_limit)
        .expect("a context exactly at the bound must be accepted");
    assert_eq!(
        std::fs::metadata(context_file(&project))
            .expect("stat the context")
            .len() as usize,
        MAX_CONTEXT_BYTES
    );
}

/// A `.dot-agent-deck` that is a regular file — not a directory and not a
/// symlink — is refused rather than clobbered.
///
/// `O_DIRECTORY` is what catches it, and it is worth its own case because
/// `create_dir` answers `AlreadyExists` for a regular file too: without the
/// directory check the publish would go on to try to create a temp file
/// *inside* it and report a confusing `TempCreate` instead.
#[test]
fn a_context_directory_that_is_a_regular_file_is_refused() {
    let (_guard, project) = project();
    std::fs::write(context_dir(&project), "not a directory").expect("seed the impostor");

    let err = publish_orchestrator_context(&project, "anything")
        .expect_err("a regular-file .dot-agent-deck must be refused");
    assert!(
        matches!(err, ContextPublishError::ContextDirUnusable(_)),
        "expected ContextDirUnusable, got {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(context_dir(&project)).expect("read the impostor"),
        "not a directory",
        "the impostor must be left exactly as it was"
    );
}

/// An existing `.dot-agent-deck` that is **acceptable** keeps whatever mode the
/// operator gave it.
///
/// The owner-only claim is about directories this code **creates**, and stating
/// it wider than that would be false: every `.dot-agent-deck` in every existing
/// checkout predates the rule, and publishing is not the operation that gets to
/// re-permission a directory somebody else made. `0o755` is what one looks like
/// under a default umask, and group/other *read* and *execute* are not what let
/// another account replace a directory entry — so this stays the accepted case
/// and the publish leaves it alone.
///
/// **What changed with PRD #819's audit is the boundary, not this side of it.**
/// The old version of this test pinned "an existing directory is accepted
/// whatever its mode", and its stated reasoning — whoever can write the parent
/// already controls `.dot-agent-deck.toml` — is true of the parent and does not
/// transfer to the child: a `.dot-agent-deck` can be group-writable while the
/// project root is not. So group/other **write** is now refused, which is
/// `a_group_or_world_writable_context_directory_is_refused`'s half of the pair.
/// Keeping both halves is the point; either alone reads as the whole rule.
#[test]
fn an_existing_acceptable_context_directory_keeps_its_mode() {
    let (_guard, project) = project();
    std::fs::create_dir(context_dir(&project)).expect("create .dot-agent-deck");
    std::fs::set_permissions(
        context_dir(&project),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("widen it the way an existing checkout would have it");

    publish_orchestrator_context(&project, "into an existing directory").expect("publish");

    assert_eq!(
        mode_of(&context_dir(&project)),
        0o755,
        "an existing directory is left as the operator had it"
    );
    assert_eq!(
        mode_of(&context_file(&project)),
        0o600,
        "the FILE is still created owner-only, whatever the directory's mode"
    );
}

/// An existing `.dot-agent-deck` that group or other can **write** is refused,
/// and nothing is published into it.
///
/// PRD #819's audit finding. A file's mode does not protect its **directory
/// entry**: another local account with write on that directory can rename or
/// replace an `orchestrator-context.md` published at `0o600`, and the next
/// coordinator then reads attacker-controlled instructions. The daemon creates
/// this directory `0o700` when it creates it at all, so the exposure is entirely
/// about directories that already exist — which the publish used to accept
/// without inspecting.
///
/// Refusing rather than re-permissioning is deliberate: `chmod`-ing a directory
/// the operator created is a side effect a publish has no business having, it
/// races the very attacker it aims at, and it hides the misconfiguration instead
/// of naming it. Both write bits are covered, because `0o770` (group only) is
/// the shape a shared-group checkout actually has, and it is every bit as
/// exposed as `0o777`.
///
/// **`0o775` is not an exotic mode, and the cost of refusing it is real.** It is
/// exactly what `mkdir` produces under a umask of `002`, which is the default on
/// hosts that give each user a private group — so a `.dot-agent-deck` left by
/// anything other than this publish (a `git` checkout, an operator's `mkdir`, or
/// this deck's own pre-M4 `create_dir_all`) is likely to carry it, and the
/// remedy is `chmod go-w .dot-agent-deck`. The check cannot distinguish a
/// private group from a shared one — group membership is not derivable from the
/// mode, and NSS makes it unreliable to look up — so it fails closed, which is
/// the same disposition OpenSSH's `StrictModes` takes for the same reason. That
/// trade is recorded here rather than discovered by whoever hits it.
///
/// Running as root would not defeat this one — the check reads the mode rather
/// than attempting a write — so unlike
/// `a_failed_publish_leaves_the_previous_context_intact_and_no_residue` it needs
/// no root guard.
#[test]
fn a_group_or_world_writable_context_directory_is_refused() {
    for mode in [0o775, 0o777, 0o707, 0o770] {
        let (_guard, project) = project();
        std::fs::create_dir(context_dir(&project)).expect("create .dot-agent-deck");
        std::fs::set_permissions(context_dir(&project), std::fs::Permissions::from_mode(mode))
            .expect("hand it out to group/other");

        let err =
            match publish_orchestrator_context(&project, "into a directory anyone can rewrite") {
                Ok(published) => {
                    panic!(
                        "mode {mode:04o} must be refused, but the publish produced {published:?}"
                    )
                }
                Err(e) => e,
            };
        let ContextPublishError::ContextDirGroupOrWorldWritable(reported) = err else {
            panic!("mode {mode:04o}: expected ContextDirGroupOrWorldWritable, got {err:?}");
        };
        assert_eq!(reported, mode, "the diagnostic names the offending mode");

        assert!(
            !context_file(&project).exists(),
            "mode {mode:04o}: nothing may be published into a directory that fails the check"
        );
        assert!(
            residue(&project).is_empty(),
            "mode {mode:04o}: and no temp file may be left behind either: {:?}",
            residue(&project)
        );
        assert_eq!(
            mode_of(&context_dir(&project)),
            mode,
            "mode {mode:04o}: the directory is refused, not silently re-permissioned"
        );
    }
}

/// A directory the publish **creates** is owner-only and therefore always passes
/// the check above — including under a permissive umask, which can only remove
/// bits.
///
/// Without this, `a_group_or_world_writable_context_directory_is_refused` would
/// be consistent with a publish that refused every directory it had just made.
#[test]
fn a_freshly_created_context_directory_satisfies_the_check_it_imposes() {
    let (_guard, project) = project();
    // SAFETY: `umask(2)` swaps a per-process value; nextest runs one process per
    // test, so nothing else here observes it. Same reasoning as
    // `creation_is_owner_only_under_a_permissive_umask_and_a_permissive_parent`.
    let previous = unsafe { libc::umask(0) };
    let published = publish_orchestrator_context(&project, "first publish in a fresh project");
    unsafe {
        libc::umask(previous);
    }
    published.expect("a directory this publish creates must satisfy its own check");

    assert_eq!(mode_of(&context_dir(&project)), 0o700);
    assert_eq!(mode_of(&context_file(&project)), 0o600);
}
