//! PRD #103 M1.0: smoke test for the `DAD_BUILD_ID` env var emitted by `build.rs`.
//!
//! Asserts that the build script produced a non-empty identifier and that it
//! starts with `DAD_VERSION` — the two invariants the handshake comparison
//! relies on (the rest of the suffix is `g<sha>[-dirty]` or `-unknown`, both
//! of which start with `<DAD_VERSION>-`).

#[test]
fn dad_build_id_is_non_empty_and_starts_with_version() {
    let build_id = env!("DAD_BUILD_ID");
    let version = env!("DAD_VERSION");

    assert!(!build_id.is_empty(), "DAD_BUILD_ID must not be empty");
    assert!(!version.is_empty(), "DAD_VERSION must not be empty");
    assert!(
        build_id.starts_with(version),
        "DAD_BUILD_ID ({build_id}) must start with DAD_VERSION ({version})"
    );
    // The suffix must be one of `-g<sha>[-dirty]` or `-unknown` — both begin
    // with the version prefix followed by a `-`.
    let suffix = &build_id[version.len()..];
    assert!(
        suffix.starts_with('-'),
        "DAD_BUILD_ID suffix must start with '-' (got {suffix:?})"
    );
}

/// PRD #103 invariant 6 regression guard. `build.rs` emits
/// `cargo:rerun-if-changed=<resolved>` for `.git/HEAD`, `.git/index`,
/// `.git/packed-refs`, and the resolved `refs/heads/<branch>`. In a git
/// worktree, those literal paths don't exist — `.git` is a *file*
/// pointing at the main repo's worktree storage — and the cargo
/// directive silently no-ops, so a stale `DAD_BUILD_ID` would survive a
/// commit on the current branch (the exact bug the PRD names).
///
/// We can't directly assert that cargo *would* rebuild on a new commit
/// (cargo's incremental machinery isn't exposed to test code). What we
/// can assert cheaply, with the same load-bearing signal, is that the
/// `DAD_BUILD_ID` that *was* baked into this test binary actually
/// reflects the current `HEAD` short-SHA. If the resolution logic ever
/// regresses to a wrong path — or a future refactor goes back to the
/// literal `.git/HEAD` and starts no-op'ing in worktrees — this test
/// pins a build_id that doesn't carry the current SHA and fails.
///
/// Skipped if `git rev-parse` isn't on PATH or this isn't a git repo
/// (tarball / shallow clone build): in those cases `compose_build_id`
/// falls back to `<version>-unknown`, which the assertion above already
/// pins.
#[test]
fn dad_build_id_carries_current_head_short_sha() {
    let build_id = env!("DAD_BUILD_ID");

    let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    else {
        eprintln!("skipping: `git rev-parse` not available");
        return;
    };
    if !output.status.success() {
        eprintln!("skipping: not a git repo or git failed");
        return;
    }
    let short_sha = String::from_utf8(output.stdout)
        .expect("git rev-parse output must be UTF-8")
        .trim()
        .to_string();
    if short_sha.is_empty() {
        eprintln!("skipping: empty short SHA");
        return;
    }

    // The build_id format is `<version>-g<short-sha>[-dirty]` or
    // `<version>-unknown`. The `-unknown` branch is only reached when
    // `git rev-parse --short HEAD` itself fails — which we just verified
    // succeeded — so we must be in the `g<short-sha>` branch.
    assert!(
        build_id.contains(&short_sha),
        "DAD_BUILD_ID ({build_id}) must contain the current HEAD short SHA ({short_sha}); \
         if you just rebased or committed and this fails, build.rs's rerun-if-changed paths \
         likely no longer resolve in the current worktree (PRD #103 invariant 6)"
    );
}
