//! The harness's own unit tests — the pure-function half of
//! `tests/common/mod.rs`: temp-root containment and name parsing, credential
//! collection and env-var trimming, the redaction range arithmetic and the
//! wrapped-credential matcher.
//!
//! **Exactly one binary compiles this file** — `tests/harness_unit.rs`, the
//! only place it is named. That is the point of it being a file rather than
//! the `#[cfg(test)] mod harness_unit_tests` it used to be inside
//! `tests/common/mod.rs`, and issue #806 is why. `cfg(test)` is true in
//! **every** integration-test target, not only in the one being tested, so a
//! `#[cfg(test)]` module inside the harness compiled into every binary that
//! wrote `mod common;` — and nextest then ran all of it, once per binary.
//! Measured on `45528bc`, the commit before the move: **108** distinct tests
//! became **7020** executions under lane 1 (`cargo test-e2e`, 65 binaries) —
//! 75.4% of everything that lane selects — 1836 in the fast tier (17
//! binaries) and 9612 under `cargo test-e2e-live` (89 binaries). The cost was
//! multiplicative, not additive: one test added here cost 65 executions in
//! every lane-1 run, forever, which is the wrong incentive on a module this
//! repo deliberately keeps well-tested.
//!
//! Nothing here needs a binary of its own to run in. These are assertions
//! about pure functions and about directories under a `tempfile` root, and
//! nextest already runs each test in its own process — so the binary a test
//! is linked into changed how many copies ran, and nothing else. Under lane 1
//! the other 64 executions proved nothing the first did not.
//!
//! **What belongs here.** Tests that are indifferent to their host binary. A
//! test that genuinely depends on what it is linked into — one asserting on
//! per-process harness state as *that* binary established it — does not,
//! because here it would run in a process that starts no deck.
//! `tests/harness_isolation.rs` is where that kind of test already lives. The
//! three tests below that re-run their own binary are fine: each derives the
//! child filter from `module_path!()`, so they followed the move rather than
//! naming a binary.
//!
//! **Why so much of `mod.rs` is `pub(crate)`.** This module is a *sibling* of
//! `common`, not a child of it, so `use super::*` no longer reaches the
//! harness's private helpers. The helpers these tests cover therefore carry
//! `pub(crate)` — 66 declarations of 64 distinct items (two are declared
//! twice, under `#[cfg]` pairs) plus four struct fields, widened for this
//! file and for nothing else. Rust offers no narrower spelling: a private
//! item cannot be re-exported (E0364/E0365), and no `#[cfg]` available here
//! distinguishes one test binary from another — `cfg(test)` is true in all of
//! them, and feature, `RUSTFLAGS` and `build.rs` cfgs are per-package rather
//! than per-target. The module tree is the only thing that can single one
//! out.

use std::path::{Path, PathBuf};

use crate::common::*;

// -----------------------------------------------------------------------
// Issue #322 — harness temp-root containment and cleanup
// -----------------------------------------------------------------------

/// Marks the root on stdout so the re-run below can capture it.
const ROOT_MARKER: &str = "harness-temp-root=";

/// The host plugin-tree copy stays off unless explicitly asked for: it is
/// ~11 MB per seeded HOME and nothing in the suite depends on it.
#[test]
fn claude_plugin_import_is_off_unless_explicitly_enabled() {
    let prev = std::env::var_os("DAD_E2E_IMPORT_CLAUDE_PLUGINS");
    // SAFETY: nextest runs one test per process, so this is single-threaded;
    // the var is restored before returning.
    unsafe { std::env::remove_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS") };
    let off_by_default = import_claude_plugins_enabled();
    unsafe { std::env::set_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS", "1") };
    let on_when_asked = import_claude_plugins_enabled();
    unsafe { std::env::set_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS", "0") };
    let off_when_zero = import_claude_plugins_enabled();
    match prev {
        Some(v) => unsafe { std::env::set_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS", v) },
        None => unsafe { std::env::remove_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS") },
    }
    assert!(!off_by_default, "plugin copy must default to off");
    assert!(on_when_asked, "=1 must re-enable the copy");
    assert!(!off_when_zero, "=0 must leave it off");
}

/// The pre-flight message names the real cause and the one command that
/// fixes it — the whole point is that a tmpfs-exhaustion run stops looking
/// like a product regression.
#[cfg(unix)]
#[test]
fn insufficient_space_message_names_the_cause_and_the_remedy() {
    let msg = insufficient_temp_space_message(312, 2048, Path::new("/tmp"));
    assert!(msg.contains("312 MB"), "missing actual free space: {msg}");
    assert!(msg.contains("2048 MB"), "missing required space: {msg}");
    assert!(msg.contains("/tmp"), "missing the filesystem: {msg}");
    assert!(
        msg.contains("cargo xtask clean-e2e-tmp --apply"),
        "missing the remedy: {msg}",
    );
    assert!(
        msg.contains("NOT a product regression"),
        "message must be impossible to mistake for a test defect: {msg}",
    );
}

/// A zero threshold disables the check, so a contributor whose temp
/// filesystem is small on purpose is never blocked by it.
#[cfg(unix)]
#[test]
fn zero_threshold_disables_the_preflight_check() {
    // SAFETY: single-threaded test process (nextest runs one test per
    // process); the var is restored before returning.
    let prev = std::env::var_os(MIN_FREE_ENV);
    unsafe { std::env::set_var(MIN_FREE_ENV, "0") };
    let verdict = temp_space_problem(Path::new("/"));
    let configured = min_free_mb();
    match prev {
        Some(v) => unsafe { std::env::set_var(MIN_FREE_ENV, v) },
        None => unsafe { std::env::remove_var(MIN_FREE_ENV) },
    }
    assert_eq!(configured, 0, "the bypass var must reach the threshold");
    assert!(verdict.is_none(), "zero threshold should disable the check");
}

/// An impossibly large threshold trips the check, proving it actually reads
/// the filesystem rather than always returning `None`.
#[cfg(unix)]
#[test]
fn an_unmeetable_threshold_trips_the_preflight_check() {
    let prev = std::env::var_os(MIN_FREE_ENV);
    // SAFETY: as above — single-threaded, restored before returning.
    unsafe { std::env::set_var(MIN_FREE_ENV, "1000000000") };
    let verdict = temp_space_problem(&std::env::temp_dir());
    match prev {
        Some(v) => unsafe { std::env::set_var(MIN_FREE_ENV, v) },
        None => unsafe { std::env::remove_var(MIN_FREE_ENV) },
    }
    assert!(
        verdict.is_some_and(|m| m.contains("clean-e2e-tmp")),
        "a 1 PB requirement should always trip the check",
    );
}

/// Room to spare is a silent pass — the decision half is exercised with
/// injected numbers so this never depends on the machine's real disk.
#[cfg(unix)]
#[test]
fn preflight_passes_when_free_space_is_above_the_threshold() {
    assert!(temp_space_verdict(Some(4096), 2048, Path::new("/var/tmp/dad-e2e-1000")).is_none());
    // Exactly at the threshold is still "enough": the comparison is `<`.
    assert!(temp_space_verdict(Some(2048), 2048, Path::new("/var/tmp/dad-e2e-1000")).is_none());
}

/// Below the threshold the verdict names the path, the requirement and the
/// shortfall — the three facts a reader needs to tell a starved harness
/// apart from a broken product.
#[cfg(unix)]
#[test]
fn preflight_fails_below_the_threshold_naming_path_required_and_found() {
    let msg = temp_space_verdict(Some(97), 2048, Path::new("/var/tmp/dad-e2e-1000"))
        .expect("97 MB is under a 2048 MB requirement");
    assert!(
        msg.contains("/var/tmp/dad-e2e-1000"),
        "missing the path: {msg}"
    );
    assert!(msg.contains("2048 MB"), "missing the requirement: {msg}");
    assert!(msg.contains("97 MB"), "missing what was found: {msg}");
    assert!(
        msg.contains("HARNESS PRE-FLIGHT FAILURE"),
        "missing the not-a-regression framing: {msg}",
    );
}

/// A filesystem whose free space cannot be queried must never fail the
/// suite — the check exists to remove a flaky failure mode, not add one.
#[cfg(unix)]
#[test]
fn preflight_degrades_gracefully_when_free_space_is_unqueryable() {
    assert!(
        temp_space_verdict(None, 2048, Path::new("/var/tmp/dad-e2e-1000")).is_none(),
        "an unqueryable filesystem must produce no verdict",
    );
    // And the query really does return `None` rather than panicking on a
    // path that is not there, so the branch above is reachable.
    assert!(
        free_bytes(Path::new("/definitely/not/a/real/mount/point-322")).is_none(),
        "statvfs on a missing path should report no answer",
    );
}

// -----------------------------------------------------------------------
// Issue #322 — the temp base lands on a short, private, disk-backed path
// -----------------------------------------------------------------------

/// The default is a private, UID-scoped parent under `/var/tmp` — short
/// enough for a socket, disk-backed by FHS convention, and owner-only so
/// nothing under it can belong to another user.
#[test]
fn temp_base_defaults_to_the_private_var_tmp_parent() {
    let parent = PathBuf::from("/var/tmp/dad-e2e-1000");
    let choice = choose_temp_base(None, Some(&parent), Path::new("/tmp"));
    assert_eq!(choice.path, parent);
    assert!(choice.warnings.is_empty(), "{:?}", choice.warnings);
}

/// An explicit `DAD_E2E_TMPDIR` outranks the private parent — that is the
/// documented escape hatch for anyone who wants a target-local or
/// otherwise unusual base.
#[test]
fn temp_base_env_override_wins_over_every_other_candidate() {
    let choice = choose_temp_base(
        Some(Path::new("/fast/scratch")),
        Some(Path::new("/var/tmp/dad-e2e-1000")),
        Path::new("/tmp"),
    );
    assert_eq!(choice.path, Path::new("/fast/scratch"));
    assert!(choice.warnings.is_empty(), "{:?}", choice.warnings);
}

/// The override is honoured even when it is too deep to bind a socket
/// under — an explicit choice is not silently overruled — but it says so.
#[test]
fn an_over_long_env_override_is_honoured_with_a_warning() {
    let deep = PathBuf::from(format!("/{}", "x".repeat(SUN_PATH_USABLE)));
    let choice = choose_temp_base(
        Some(&deep),
        Some(Path::new("/var/tmp/dad-e2e-1000")),
        Path::new("/tmp"),
    );
    assert_eq!(choice.path, deep);
    let warning = choice.warnings.first().expect("an unusable override warns");
    assert!(warning.contains(TEMP_BASE_ENV), "{warning}");
    assert!(warning.contains("AF_UNIX path too long"), "{warning}");
}

/// With no usable private parent the system temp dir is the last resort —
/// and because that is the RAM-backed outcome issue #322 is about, it is
/// the one case that always warns.
#[test]
fn the_system_temp_dir_is_a_last_resort_and_says_so() {
    let choice = choose_temp_base(None, None, Path::new("/tmp"));
    assert_eq!(choice.path, Path::new("/tmp"));
    let warning = choice.warnings.first().expect("falling back to /tmp warns");
    assert!(warning.contains("#322"), "{warning}");
    assert!(warning.contains(TEMP_BASE_ENV), "{warning}");
}

/// The length veto applies to the private parent too — an absurd UID (or a
/// future longer parent name) must degrade rather than produce a base no
/// socket can be bound under.
#[test]
fn an_over_long_private_parent_is_vetoed_like_any_other_candidate() {
    let parent = PathBuf::from(format!("/var/tmp/{}", "u".repeat(MAX_TEMP_BASE_LEN)));
    let choice = choose_temp_base(None, Some(&parent), Path::new("/tmp"));
    assert_eq!(choice.path, Path::new("/tmp"));
    let warning = choice.warnings.first().expect("a vetoed parent warns");
    assert!(warning.contains(&parent.display().to_string()), "{warning}");
}

/// The real parent this machine would use is owner-only and ours. The
/// structural claim the whole `/var/tmp` rung rests on: `/var/tmp` is mode
/// 1777, so without a verified 0700 parent a `dad-tests-*` directory there
/// could belong to anybody.
#[cfg(unix)]
#[test]
fn the_private_parent_is_owner_only_and_owned_by_us() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let parent = match private_temp_parent() {
        Ok(p) => p,
        Err(why) => {
            eprintln!("skipping: no private parent available here ({why})");
            return;
        }
    };
    assert_eq!(
        parent.file_name().and_then(|n| n.to_str()),
        Some(private_parent_name(effective_uid()).as_str()),
        "the parent must be scoped to the effective UID",
    );
    let meta = std::fs::symlink_metadata(&parent).expect("stat private parent");
    assert!(
        !meta.file_type().is_symlink(),
        "parent must not be a symlink"
    );
    let mode = meta.permissions().mode() & 0o7777;
    assert_eq!(
        private_dir_objection(meta.uid(), mode, effective_uid()),
        None,
        "{} is uid {} mode 0o{mode:o}",
        parent.display(),
        meta.uid(),
    );
}

/// A parent that is not ours is refused, not repaired: chmod'ing or
/// chown'ing someone else's directory is exactly the behaviour that makes a
/// shared `/var/tmp` dangerous.
#[cfg(unix)]
#[test]
fn a_foreign_or_loose_private_parent_is_refused() {
    assert!(
        private_dir_objection(1001, 0o700, 1000).is_some(),
        "a directory owned by another uid must be refused",
    );
    assert!(
        private_dir_objection(1000, 0o750, 1000).is_some(),
        "a group-readable directory must be refused",
    );
    assert!(
        private_dir_objection(1000, 0o1777, 1000).is_some(),
        "a world-writable directory must be refused",
    );
    assert_eq!(private_dir_objection(1000, 0o700, 1000), None);
}

/// The predicate enforces the **exact** 0o700 that the diagnostics, the
/// audit note and `docs/develop/e2e-temp-dirs.md` all claim.
///
/// It used to test only `mode & 0o077 == 0`, which 0o500, 0o300, 0o000 and
/// 0o1700 also satisfy. Confidentiality was never the gap — `mkdir(2)`
/// applies the mode and a umask can only clear bits — but a pre-existing
/// 0o500 parent passed the pre-flight whose whole job is to name the problem
/// up front, and then failed much later as a bare `Permission denied` from
/// somewhere inside a test. So the check now matches the claim, and the
/// message has to name the innocent cause: a umask that clears owner bits.
#[cfg(unix)]
#[test]
fn the_private_dir_rule_requires_exactly_0o700_not_merely_owner_only() {
    assert_eq!(private_dir_objection(1000, 0o700, 1000), None);

    // Owner bits missing: no confidentiality problem, but not usable, and
    // previously accepted.
    for mode in [0o500, 0o300, 0o600, 0o000] {
        let why = private_dir_objection(1000, mode, 1000)
            .unwrap_or_else(|| panic!("0o{mode:o} must be refused"));
        assert!(why.contains(&format!("mode is 0o{mode:o}")), "{why}");
        assert!(
            why.contains("umask"),
            "0o{mode:o} must name the cause: {why}"
        );
    }

    // Sticky-but-owner-only — 0o1700 — was accepted by the old mask too.
    let why = private_dir_objection(1000, 0o1700, 1000).expect("0o1700 must be refused");
    assert!(why.contains("mode is 0o1700"), "{why}");

    // Group/other bits get the other half of the message: what is at risk.
    let why = private_dir_objection(1000, 0o750, 1000).expect("0o750 must be refused");
    assert!(why.contains("credentials"), "{why}");
}

/// Refusal must be **fatal**, not a warning. Refusing the directory and
/// then dropping to `std::env::temp_dir()` converts a security refusal into
/// issue #322's original capacity problem, and the only signal is a stderr
/// line nextest interleaves across thousands of processes.
///
/// Asserted on the pure verdict, because the foreign-owned shape cannot be
/// built on disk without `chown`. Every claim the message has to make is
/// pinned: what it is, that nothing ran, the path, observed state, required
/// state, the remedy, and the escape hatch.
#[cfg(unix)]
#[test]
fn a_refused_private_parent_is_fatal_and_actionable() {
    let path = Path::new("/var/tmp/dad-e2e-1000");
    let msg = private_parent_verdict(path, false, true, 1001, 0o755, 1000)
        .expect("a foreign-owned, group-readable parent must be refused");
    for expected in [
        "HARNESS PRE-FLIGHT FAILURE",
        "NOT a product regression",
        "No test has run.",
        "/var/tmp/dad-e2e-1000 exists and is",
        // observed …
        "a directory owned by uid 1001 with mode 0o755",
        // … versus required
        "requires a real directory owned by uid 1000 with mode 0o700",
        // why it is not falling back rather than just that it is not
        "RAM-backed tmpfs",
        "#322",
        // the remedy, and the way out for someone who cannot take it
        "ls -ld /var/tmp/dad-e2e-1000",
        "rm -rf /var/tmp/dad-e2e-1000",
        TEMP_BASE_ENV,
    ] {
        assert!(msg.contains(expected), "missing {expected:?} in:\n{msg}");
    }
}

/// Each refusable shape produces a verdict naming what was seen, and a
/// parent that is exactly what the harness asks for produces none — so the
/// new hard failure cannot fire on a healthy machine.
#[cfg(unix)]
#[test]
fn every_untrustworthy_parent_shape_earns_a_verdict_and_a_good_one_does_not() {
    let path = Path::new("/var/tmp/dad-e2e-1000");
    let observed = |is_symlink, is_dir, uid, mode| {
        private_parent_verdict(path, is_symlink, is_dir, uid, mode, 1000)
    };
    assert!(
        observed(true, false, 1000, 0o700).is_some_and(|m| m.contains("exists and is a symlink")),
        "a symlink at the parent's name must be refused",
    );
    assert!(
        observed(false, false, 1000, 0o600)
            .is_some_and(|m| m.contains("exists and is not a directory")),
        "a plain file at the parent's name must be refused",
    );
    assert!(
        observed(false, true, 1000, 0o750)
            .is_some_and(|m| m.contains("owned by uid 1000 with mode 0o750")),
        "group bits must be refused",
    );
    assert_eq!(
        observed(false, true, 1000, 0o700),
        None,
        "the parent this machine actually has must not be refused",
    );
}

/// Unwrap a [`private_temp_parent_in`] outcome that must be a hard refusal,
/// failing loudly on either of the two ways it could be wrong: adopting the
/// directory, or degrading to a warning and the next rung of the ladder.
#[cfg(unix)]
fn refusal_message(outcome: Result<PathBuf, PrivateParentProblem>) -> String {
    match outcome {
        Ok(p) => panic!("{} was adopted; it should have been refused", p.display()),
        Err(PrivateParentProblem::Unavailable(why)) => {
            panic!("degraded to a warning and fell through the ladder: {why}")
        }
        Err(PrivateParentProblem::Refused(message)) => message,
    }
}

/// The classification is made against what is really on disk, not just in
/// the pure verdict. Three of the four refusable shapes can be built
/// without privileges — a symlink, a plain file, and a loosened mode — and
/// each must come back `Refused` rather than falling through.
#[cfg(unix)]
#[test]
fn a_present_but_untrustworthy_parent_is_refused_on_disk() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    let euid = effective_uid();
    let name = private_parent_name(euid);
    // Each shape gets its own stand-in for `/var/tmp`, since the parent's
    // name inside it is fixed by the UID.
    let shared = |kind: &str| {
        let dir = anchor.path().join(kind);
        std::fs::create_dir(&dir).expect("stand-in /var/tmp");
        dir
    };

    let linked = shared("symlink");
    let target = anchor.path().join("elsewhere");
    std::fs::create_dir(&target).expect("link target");
    std::os::unix::fs::symlink(&target, linked.join(&name)).expect("plant a symlink");
    let msg = refusal_message(private_temp_parent_in(&linked, euid));
    assert!(msg.contains("exists and is a symlink"), "{msg}");

    let filed = shared("file");
    std::fs::write(filed.join(&name), b"not a directory").expect("plant a file");
    let msg = refusal_message(private_temp_parent_in(&filed, euid));
    assert!(msg.contains("exists and is not a directory"), "{msg}");

    // `set_permissions` rather than a creation mode: the umask can only
    // clear bits, so a 0o755 asked for at `mkdir` time is not guaranteed.
    let loose = shared("loose");
    let parent = loose.join(&name);
    std::fs::create_dir(&parent).expect("plant a directory");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
        .expect("loosen the mode");
    let msg = refusal_message(private_temp_parent_in(&loose, euid));
    assert!(
        msg.contains(&format!("owned by uid {euid} with mode 0o755")),
        "{msg}",
    );
    assert!(msg.contains(&parent.display().to_string()), "{msg}");

    // Refused, not repaired: the offending directory is untouched.
    let mode = std::fs::symlink_metadata(&parent)
        .expect("stat the refused parent")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o755, "the refused parent was modified");
}

/// Making refusal fatal must not break a machine that simply has no
/// `/var/tmp`. An absent shared directory is an ordinary environment
/// difference, so it stays `Unavailable` and the ladder falls through to
/// the last resort with the warning it has always printed.
#[cfg(unix)]
#[test]
fn an_absent_shared_directory_still_falls_through_the_ladder() {
    let anchor = race_safe_tempdir();
    let missing = anchor.path().join("no-var-tmp-here");
    match private_temp_parent_in(&missing, effective_uid()) {
        Err(PrivateParentProblem::Unavailable(why)) => {
            assert!(why.contains(&missing.display().to_string()), "{why}");
        }
        Ok(p) => panic!("{} does not exist and must not be created", p.display()),
        Err(PrivateParentProblem::Refused(msg)) => {
            panic!("an absent shared directory must never be fatal:\n{msg}")
        }
    }
    // And that outcome is exactly the `None` the ladder already handles.
    let choice = choose_temp_base(None, None, Path::new("/tmp"));
    assert_eq!(choice.path, Path::new("/tmp"));
    assert!(
        choice.warnings.first().is_some_and(|w| w.contains("#322")),
        "{:?}",
        choice.warnings,
    );
}

/// The ordinary path still works through the new seam: a fresh shared
/// directory gets an owner-only parent created under it, and a second call
/// adopts what the first created rather than objecting to it.
#[cfg(unix)]
#[test]
fn a_fresh_shared_directory_yields_an_owner_only_parent() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    let euid = effective_uid();
    let parent = private_temp_parent_in(anchor.path(), euid).expect("a fresh parent");
    assert_eq!(parent, anchor.path().join(private_parent_name(euid)));
    let mode = std::fs::symlink_metadata(&parent)
        .expect("stat the created parent")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o700, "{} is 0o{mode:o}", parent.display());
    assert_eq!(
        private_temp_parent_in(anchor.path(), euid).ok(),
        Some(parent),
        "adopting the parent it just created must not object",
    );
}

/// macOS reaches `/var/tmp` through a symlink — `/var -> private/var` — so
/// the harness and `cargo xtask clean-e2e-tmp` end up holding two different
/// spellings of one directory. This pins which one the *harness* holds.
///
/// [`private_temp_parent_in`] joins the parent's name onto the shared
/// directory it was handed and never canonicalises, so what the socket
/// budget is charged against, and what a later `bind(2)` actually sees, is
/// the short `/var/tmp/dad-e2e-<uid>`. The reaper resolves instead and scans
/// `/private/var/tmp/dad-e2e-<uid>`. The only thing that has to be true of
/// the pair is that they are one directory, which is asserted by inode
/// rather than by string.
#[cfg(unix)]
#[test]
fn the_private_parent_keeps_the_short_spelling_the_socket_budget_charges_for() {
    use std::os::unix::fs::MetadataExt;
    let anchor = race_safe_tempdir();
    let euid = effective_uid();
    // macOS's own layout in miniature: a real `private/var/tmp`, reached
    // through a `var` symlink that `lstat` traverses because it is never the
    // final component.
    std::fs::create_dir_all(anchor.path().join("private/var/tmp"))
        .expect("stand-in /private/var/tmp");
    std::os::unix::fs::symlink("private/var", anchor.path().join("var")).expect("plant /var");
    let shared = anchor.path().join("var/tmp");

    let by_name = private_temp_parent_in(&shared, euid).expect("a parent below the link");
    assert_eq!(by_name, shared.join(private_parent_name(euid)));

    let resolved = by_name.canonicalize().expect("resolve the parent");
    assert_ne!(
        by_name, resolved,
        "the fixture must really diverge, as macOS does",
    );
    assert!(
        by_name.as_os_str().len() < resolved.as_os_str().len(),
        "the harness must hold the shorter spelling: {} vs {}",
        by_name.display(),
        resolved.display(),
    );
    let named = std::fs::metadata(&by_name).expect("stat by name");
    let followed = std::fs::metadata(&resolved).expect("stat resolved");
    assert_eq!(
        (named.dev(), named.ino()),
        (followed.dev(), followed.ino()),
        "the two spellings must be one directory",
    );
}

/// The socket budget at a macOS UID, in both spellings the two halves use.
///
/// [`SUN_PATH_USABLE`] is already macOS's 103, so the only open question is
/// whether a `501`-shaped parent composes inside it. Both do, with room:
/// `/var/tmp/dad-e2e-501` is 20 bytes and composes to 68;
/// `/private/var/tmp/dad-e2e-501` is 28 and composes to 76 — against a
/// 55-byte base allowance and a 103-byte socket path. The harness binds
/// under the first of those, and the veto is applied to that same value.
#[cfg(unix)]
#[test]
fn a_macos_uid_fits_the_socket_budget_in_both_spellings() {
    let name = private_parent_name(501);
    let by_name = PathBuf::from(SHARED_VAR_TMP).join(&name);
    let resolved = PathBuf::from("/private")
        .join(SHARED_VAR_TMP.trim_start_matches('/'))
        .join(&name);
    assert_eq!(by_name, Path::new("/var/tmp/dad-e2e-501"));
    assert_eq!(resolved, Path::new("/private/var/tmp/dad-e2e-501"));

    for (base, len) in [(&by_name, 20), (&resolved, 28)] {
        assert_eq!(base.as_os_str().len(), len, "{}", base.display());
        assert!(
            fits_socket_budget(base),
            "{} ({len} bytes) exceeds the {MAX_TEMP_BASE_LEN}-byte allowance",
            base.display(),
        );
        assert!(
            len + HARNESS_SOCKET_OVERHEAD <= SUN_PATH_USABLE,
            "{} composes to {} bytes, past {SUN_PATH_USABLE}",
            base.display(),
            len + HARNESS_SOCKET_OVERHEAD,
        );
    }

    // And the ladder picks the short one without complaint — the veto sees
    // exactly the value that reaches `bind(2)`.
    let choice = choose_temp_base(None, Some(&by_name), Path::new("/tmp"));
    assert_eq!(choice.path, by_name);
    assert!(choice.warnings.is_empty(), "{:?}", choice.warnings);
}

/// A refused `DAD_E2E_TMPDIR` is fatal too, and for a stronger reason than
/// the default: the operator stated where the temp dirs must go, so quietly
/// putting them somewhere else is both wrong and unasked-for.
#[test]
fn a_refused_env_override_is_fatal_rather_than_ignored() {
    let raw = Path::new("scratch/e2e");
    let why = override_shape_objection(raw).expect("a relative value is refused");
    let msg = refused_override_message(raw, &why);
    let named = format!("{TEMP_BASE_ENV}=scratch/e2e cannot be used");
    let unset_default = format!("unset it to use the default {SHARED_VAR_TMP}/dad-e2e-<uid>");
    for expected in [
        "HARNESS PRE-FLIGHT FAILURE",
        "NOT a product regression",
        "No test has run.",
        named.as_str(),
        "is not an absolute path",
        "RAM-backed tmpfs",
        "#322",
        unset_default.as_str(),
    ] {
        assert!(msg.contains(expected), "missing {expected:?} in:\n{msg}");
    }
}

/// Traversal is judged by a laxer rule than ownership of the base itself:
/// `/`, `/home` and `/var` are root-owned, and sticky 1777 directories are
/// safe because only an entry's owner can rename or remove it.
#[cfg(unix)]
#[test]
fn override_ancestors_allow_root_owned_and_sticky_components() {
    assert_eq!(traversal_objection(0, 0o755, 1000), None, "root-owned /usr");
    assert_eq!(
        traversal_objection(0, 0o1777, 1000),
        None,
        "sticky /var/tmp"
    );
    assert_eq!(traversal_objection(1000, 0o700, 1000), None, "our own dir");
    assert!(
        traversal_objection(0, 0o777, 1000).is_some(),
        "world-writable without the sticky bit is the swappable case",
    );
    assert!(
        traversal_objection(1001, 0o755, 1000).is_some(),
        "a component owned by another unprivileged user must be refused",
    );
}

/// A relative value, or one with `..` in it, is refused rather than
/// normalised: relative resolves against whatever working directory the
/// test binary happens to have, and `..` silently widens the scope of
/// everything downstream — including what the reaper would be pointed at.
/// A `.` is a different matter: it is not a widening, and `components()`
/// removes it before anything sees the path.
///
/// What counts as absolute is a *platform* question, and the fixtures are
/// split accordingly: `/var/tmp/e2e` is absolute on Unix and is not on
/// Windows, where an absolute path needs a drive letter. The rule is one
/// rule — `Path::is_absolute` — asserted against each platform's own
/// spelling of it.
#[test]
fn an_override_that_is_relative_or_traversing_is_refused() {
    // True everywhere: a bare relative path is absolute on no platform.
    assert!(override_shape_objection(Path::new("scratch/e2e")).is_some());
    assert!(override_shape_objection(Path::new("./scratch/e2e")).is_some());
    #[cfg(unix)]
    {
        assert!(override_shape_objection(Path::new("/var/tmp/../../etc")).is_some());
        assert_eq!(override_shape_objection(Path::new("/var/tmp/e2e")), None);
        assert_eq!(override_shape_objection(Path::new("/var/tmp/./e2e")), None);
    }
    #[cfg(windows)]
    {
        // Rooted but not absolute: no drive letter, so it resolves against
        // whatever drive is current — exactly the ambiguity being refused.
        assert!(override_shape_objection(Path::new(r"\scratch\e2e")).is_some());
        assert!(override_shape_objection(Path::new(r"C:\tmp\..\..\Windows")).is_some());
        assert_eq!(override_shape_objection(Path::new(r"C:\tmp\e2e")), None);
        assert_eq!(override_shape_objection(Path::new(r"C:\tmp\.\e2e")), None);
    }
}

/// The directory a scratch anchor really lives at, with every symlink in it
/// resolved. On macOS `/var` is a symlink to `/private/var`, so the harness
/// roots these tests build under are reached through one on a completely
/// healthy machine — and [`validated_override_base`] returns the resolved
/// spelling, which is what the assertions below have to compare against.
#[cfg(unix)]
fn resolved(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()))
}

/// The value is resolved exactly once, into a normalized, symlink-free path.
/// A spelling the filesystem does not use would otherwise be what the
/// socket-length budget is measured against, and what every later message
/// names.
#[cfg(unix)]
#[test]
fn a_validated_override_base_comes_back_normalized() {
    let anchor = race_safe_tempdir();
    let noisy = anchor.path().join(".").join("base");
    let base = validated_override_base(&noisy).expect("a fresh base under our own dir");
    assert_eq!(base, resolved(anchor.path()).join("base"));
}

/// A symlinked *ancestor* is resolved, not refused — and the resolved form
/// is what comes back, so nothing downstream ever walks the link again.
///
/// This is the macOS case: `/var` is a symlink to `/private/var` there, so
/// `std::env::temp_dir()` and everything under `/var/tmp` has a symlinked
/// ancestor on a healthy machine, and refusing symlinked components outright
/// rejected the platform's own temp directory. A root-owned system symlink
/// is not the threat; a component an unprivileged attacker could plant or
/// swap is, and that is judged after resolution — see the two tests below.
#[cfg(unix)]
#[test]
fn an_override_reached_through_a_symlink_resolves_to_the_real_directory() {
    use std::os::unix::fs::DirBuilderExt;
    let anchor = race_safe_tempdir();
    let real = anchor.path().join("real");
    // Owner-only, because this ends up an *ancestor* of the base: a bare
    // `create_dir` under `umask 002` is 0775, and a group-writable ancestor
    // is refused on its own merits — a different rule from the one under
    // test here.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&real)
        .expect("create real dir");
    let link = anchor.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("create symlink");
    let base = validated_override_base(&link.join("base")).expect("a symlinked ancestor");
    assert_eq!(base, resolved(&real).join("base"));
    assert!(base.is_dir(), "{} was not created", base.display());
    assert!(
        !base.starts_with(&link),
        "{} still names the link {}",
        base.display(),
        link.display(),
    );
}

/// The blocker the first cut of this walk had: a symlink's **owner** is
/// checked before the link is followed, not after it has been resolved away.
///
/// Driven through the pure decision because the dangerous shape needs
/// `chown` to build — the whole point is a link owned by *somebody else*.
/// The follow path itself is exercised on disk by the tests below.
///
/// This is the case `canonicalize` silently ate. On a multi-user host the
/// victim asks for `DAD_E2E_TMPDIR=/var/tmp/my-dad/base`; before their first
/// run another user creates `/var/tmp/my-dad` as a symlink to the victim's
/// own checkout. `/var/tmp` is sticky, so the victim cannot remove or rename
/// that entry — sticky protects the *attacker's* planted link here — and
/// resolving first meant the checkout was then walked as a chain of
/// perfectly ordinary victim-owned ancestors and accepted, with `base`
/// created 0700 inside the live repository.
#[cfg(unix)]
#[test]
fn a_symlink_owned_by_another_user_is_refused_before_it_is_followed() {
    let path = Path::new("/var/tmp/my-dad");

    // Ours: the operator naming their own directory through their own link.
    assert_eq!(symlink_hop_objection(path, 1000, 1000), None);
    // Root's: macOS's `/var -> private/var`, which refusing would reject the
    // whole platform.
    assert_eq!(symlink_hop_objection(path, 0, 1000), None);

    let why = symlink_hop_objection(path, 1001, 1000).expect("a foreign link is refused");
    assert!(why.contains("symlink owned by uid 1001"), "{why}");
    assert!(why.contains("neither 1000 nor root"), "{why}");
    assert!(why.contains("/var/tmp/my-dad"), "{why}");
}

/// The sticky-directory case end to end, with the shapes that *can* be built
/// unprivileged: a sticky 1777 stand-in for `/var/tmp` is traversed, and a
/// link inside it that we own is followed rather than refused.
///
/// Together with the pure test above this pins both halves of the rule —
/// that a sticky ancestor is still accepted (it has to be: `/var/tmp` is
/// 1777 on every real machine), and that acceptance now depends on judging
/// the entry found *below* it rather than on the sticky bit alone.
#[cfg(unix)]
#[test]
fn a_link_we_own_under_a_sticky_directory_is_followed() {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    // The `/var/tmp` stand-in: world-writable, sticky. Another local user
    // could create entries here; the sticky bit only stops them removing
    // ours.
    let shared = anchor.path().join("shared");
    std::fs::create_dir(&shared).expect("create the sticky stand-in");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).expect("chmod 1777");

    let real = shared.join("real");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&real)
        .expect("create the real target");
    let link = shared.join("my-dad");
    std::os::unix::fs::symlink(&real, &link).expect("plant a link we own");

    let base = validated_override_base(&link.join("base")).expect("our own link is followed");
    assert_eq!(base, resolved(&real).join("base"));
    assert!(base.is_dir(), "{} was not created", base.display());
}

/// A **non-final** link is judged too, before the tail below it is created.
///
/// The redirection in the finding is not a link at the base — it is a link
/// at an ancestor whose missing tail the harness would then happily create
/// on the far side of it. This walks that exact shape with a link we own
/// (the only owner a test can produce) and pins the two things that must be
/// true regardless: the link is resolved *hop by hop* through descriptors,
/// and the resolved spelling — never the link's own — is what comes back and
/// is therefore what every downstream message, length budget and reaper
/// hint sees.
#[cfg(unix)]
#[test]
fn a_non_final_link_is_resolved_before_its_missing_tail_is_created() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    // A 0o755 stand-in for a checkout: ours, ordinary, and a perfectly legal
    // *ancestor* — which is exactly why the link pointing at it has to be
    // the thing that is judged.
    let checkout = anchor.path().join("checkout");
    std::fs::create_dir(&checkout).expect("create the checkout");
    std::fs::set_permissions(&checkout, std::fs::Permissions::from_mode(0o755))
        .expect("chmod 0755");
    let link = anchor.path().join("link");
    std::os::unix::fs::symlink(&checkout, &link).expect("plant the link");

    let base = validated_override_base(&link.join("outer").join("inner"))
        .expect("our own link is followed");
    assert_eq!(
        base,
        resolved(&checkout).join("outer").join("inner"),
        "the resolved spelling must come back, never the link's",
    );
    assert!(
        !base.starts_with(&link),
        "{} still names the link {}",
        base.display(),
        link.display(),
    );
    // The tail below the link is still created owner-only, one component at
    // a time — a permissive directory on the far side does not relax it.
    for component in [base.parent().expect("outer").to_path_buf(), base] {
        let mode = std::fs::symlink_metadata(&component)
            .expect("stat created component")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode,
            0o700,
            "{} was created 0o{mode:o}",
            component.display(),
        );
    }
}

/// A **dangling** link we own is followed and its target created, rather
/// than refused.
///
/// This changed with the walk, so it is pinned rather than left implicit.
/// Before, `canonicalize` failed with `NotFound` on the dangling component,
/// the name went into the "missing" list, `mkdirat` came back `EEXIST` and
/// the value was rejected as "is a symlink". Now the link is judged on its
/// own merits first, and one owned by us or by root is resolved — so
/// pointing `DAD_E2E_TMPDIR` through a link whose target does not exist yet
/// creates the target, which is the same thing the harness does for any
/// other base that is not there yet. Safety is unchanged: the link's owner
/// gates the hop, and every component of the target is walked and judged.
///
/// The one place a link is still refused outright is
/// [`create_or_adopt_component`] — a link that appears in a slot the walk
/// had just found *empty* is the adoption race, not an operator's choice.
#[cfg(unix)]
#[test]
fn a_dangling_link_we_own_is_followed_and_its_target_created() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    let target = anchor.path().join("not-there-yet");
    let link = anchor.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("plant a dangling link");

    let base = validated_override_base(&link).expect("our own dangling link is followed");
    assert_eq!(base, resolved(anchor.path()).join("not-there-yet"));
    let mode = std::fs::symlink_metadata(&base)
        .expect("stat the created target")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o700, "{} is 0o{mode:o}", base.display());
}

/// A link chain that loops is bounded rather than spun on. `canonicalize`
/// used to return `ELOOP` for this; now that the harness resolves links
/// itself, the cap has to be its own.
#[cfg(unix)]
#[test]
fn a_symlink_cycle_is_refused_rather_than_followed_forever() {
    let anchor = race_safe_tempdir();
    let a = anchor.path().join("a");
    let b = anchor.path().join("b");
    std::os::unix::fs::symlink(&b, &a).expect("a -> b");
    std::os::unix::fs::symlink(&a, &b).expect("b -> a");
    let err = validated_override_base(&a.join("base")).expect_err("a cycle is refused");
    assert!(err.contains("more than 40 symlinks"), "{err}");
}

/// A `..` inside a link *target* would step back above a component the walk
/// has already proved safe, so it is refused rather than resolved. No system
/// link the harness needs contains one — macOS's `/var -> private/var` does
/// not.
#[cfg(unix)]
#[test]
fn a_link_target_containing_a_parent_component_is_refused() {
    use std::os::unix::fs::DirBuilderExt;
    let anchor = race_safe_tempdir();
    // Owner-only: this is an *ancestor* of the link, and a bare
    // `create_dir` under `umask 002` is 0775 — group-writable, which is
    // refused on its own merits, a different rule from the one under test.
    let outer = anchor.path().join("outer");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&outer)
        .expect("create outer");
    let link = outer.join("up");
    std::os::unix::fs::symlink("../sibling", &link).expect("plant a `..` link");
    let err = validated_override_base(&link.join("base")).expect_err("`..` in a target");
    assert!(err.contains("contains a `..` component"), "{err}");
}

/// Resolving a symlink does not lower the bar for what it resolves *to*: a
/// link is a fine way to name a directory and a terrible way to inherit
/// trust, so the target is judged exactly as if it had been named directly.
#[cfg(unix)]
#[test]
fn a_symlink_target_is_judged_like_any_other_directory() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    let target = anchor.path().join("target");
    std::fs::create_dir(&target).expect("create the target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .expect("loosen the target");
    let link = anchor.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");
    let err = validated_override_base(&link).expect_err("a group-readable target is refused");
    assert!(err.contains("mode is 0o755"), "{err}");
    assert!(
        err.contains(&resolved(&target).display().to_string()),
        "{err}"
    );
}

/// A base that does not exist yet is created owner-only, one component at a
/// time, rather than `create_dir_all`-ed at the umask default.
#[cfg(unix)]
#[test]
fn a_missing_override_base_is_created_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    let base = anchor.path().join("outer").join("inner");
    let created = validated_override_base(&base).expect("a fresh base under our own dir");
    assert_eq!(created, resolved(anchor.path()).join("outer").join("inner"));
    for component in [created.parent().expect("outer").to_path_buf(), created] {
        let mode = std::fs::metadata(&component)
            .expect("stat created component")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode,
            0o700,
            "{} was created 0o{mode:o}, not owner-only",
            component.display(),
        );
    }
}

/// The base is held to the same bar whether the harness created it or found
/// it: it is where real agent credentials get seeded, so ours-and-owner-only
/// is the point, and an ancestor's laxer rule does not apply to it. Refused,
/// never repaired — chmod'ing a directory the harness does not own is what
/// this whole check exists to avoid.
#[cfg(unix)]
#[test]
fn an_existing_override_base_must_still_be_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    let base = anchor.path().join("base");
    std::fs::create_dir(&base).expect("plant the base");
    let mode_of = |p: &Path| {
        std::fs::symlink_metadata(p)
            .expect("stat the base")
            .permissions()
            .mode()
            & 0o7777
    };

    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755))
        .expect("loosen the base");
    let err = validated_override_base(&base).expect_err("a group-readable base is refused");
    assert!(err.contains("mode is 0o755"), "{err}");
    assert!(
        err.contains(&resolved(&base).display().to_string()),
        "{err}"
    );
    assert_eq!(mode_of(&base), 0o755, "the refused base was modified");

    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
        .expect("tighten the base");
    assert_eq!(
        validated_override_base(&base).expect("an owner-only base is adopted"),
        resolved(&base),
    );
}

/// The adoption race (Greptile P1). A component that was missing when the
/// path was resolved can be created by another local user before the harness
/// gets to it, and a recursive create would adopt whatever is there — their
/// directory, or their symlink — without ever looking at it.
///
/// Winning a real race in a test is not practical, so what is pinned is the
/// *decision*: planting the entry before the call reproduces exactly the
/// state losing that race leaves behind. Each shape an attacker could leave
/// must be refused on the descriptor the harness actually opened, whatever
/// an earlier stat of the name said.
#[cfg(unix)]
#[test]
fn a_component_that_appears_before_creation_is_judged_not_adopted() {
    use std::os::unix::fs::PermissionsExt;
    let anchor = race_safe_tempdir();
    let euid = effective_uid();
    // Each shape needs its own parent, since they all plant the same name.
    let parent_of = |kind: &str| -> (PathBuf, std::fs::File) {
        let dir = anchor.path().join(kind);
        std::fs::create_dir(&dir).expect("stand-in parent");
        let handle = std::fs::File::open(&dir).expect("open the parent");
        (dir, handle)
    };
    let adopt = |path: &Path, handle: &std::fs::File| {
        let mut walked = path.to_path_buf();
        create_or_adopt_component(handle, std::ffi::OsStr::new("base"), &mut walked, euid)
            .map(|_| walked)
    };

    // Nothing there: created by `mkdirat` at 0o700 and accepted.
    let (fresh, handle) = parent_of("fresh");
    let made = adopt(&fresh, &handle).expect("a fresh component is created");
    assert_eq!(made, fresh.join("base"));
    let mode = std::fs::symlink_metadata(&made)
        .expect("stat the created component")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o700, "{} is 0o{mode:o}", made.display());

    // A directory that is ours but not owner-only — the shape a lost race
    // leaves when the winner made it world-readable.
    let (loose, handle) = parent_of("loose");
    let planted = loose.join("base");
    std::fs::create_dir(&planted).expect("plant a directory");
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).expect("loosen it");
    let err = adopt(&loose, &handle).expect_err("a loose directory is refused");
    assert!(err.contains("mode is 0o755"), "{err}");
    assert!(err.contains(&planted.display().to_string()), "{err}");

    // A symlink at the name: `O_NOFOLLOW` refuses it, and — the part that
    // matters — nothing is created at the far end of it.
    let (linked, handle) = parent_of("symlink");
    let target = anchor.path().join("symlink-target");
    std::fs::create_dir(&target).expect("link target");
    std::os::unix::fs::symlink(&target, linked.join("base")).expect("plant a symlink");
    let err = adopt(&linked, &handle).expect_err("a symlink is refused");
    assert!(err.contains("is a symlink"), "{err}");
    assert_eq!(
        std::fs::read_dir(&target)
            .expect("read the link target")
            .count(),
        0,
        "the symlink was followed and written through",
    );

    // A plain file at the name.
    let (filed, handle) = parent_of("file");
    std::fs::write(filed.join("base"), b"not a directory").expect("plant a file");
    let err = adopt(&filed, &handle).expect_err("a file is refused");
    assert!(err.contains("is not a directory"), "{err}");
}

/// The same adoption race in the `#[cfg(not(unix))]` arm (Greptile P1 on
/// #472), which used to hand the whole unchecked pathname to
/// `create_dir_all` and take whatever was there.
///
/// [`override_base_by_name`] is what that arm now calls, and it is
/// deliberately `cfg`-free so this runs on the Unix host the suite actually
/// executes on: the logic is plain `std::fs`, so what is observed here is
/// what Windows executes. As on the Unix side, winning a real race in a test
/// is not practical, so what is pinned is the *decision* — planting the entry
/// before the call reproduces exactly the state losing that race leaves
/// behind.
///
/// The last case pins a **limit, not a guarantee**: a plain directory
/// somebody else planted is still adopted, because `std` exposes no owner on
/// Windows. Asserting it keeps the residual honest rather than implied.
#[test]
fn a_by_name_override_component_is_created_or_refused_never_adopted() {
    // Held for the whole test: dropping it removes the tree underneath.
    let guard = race_safe_tempdir();
    // Resolved, because this walk refuses a symlinked ancestor rather than
    // following it (see [`override_base_by_name`]) and on macOS `/var` — the
    // harness root's own parent — is a symlink to `/private/var`. That is a
    // property of the *fixture path*, not of the rule under test.
    let anchor = guard
        .path()
        .canonicalize()
        .expect("resolve the scratch anchor");
    // Each shape needs its own parent, since they all plant the same name.
    let parent_of = |kind: &str| -> PathBuf {
        let dir = anchor.join(kind);
        std::fs::create_dir(&dir).expect("stand-in parent");
        dir
    };

    // Nothing there: the ordinary path still works, through a component that
    // does not exist either.
    let fresh = parent_of("fresh").join("outer").join("base");
    let made = override_base_by_name(&fresh).expect("a fresh base is created");
    assert_eq!(made, fresh, "the value comes back as the normalized path");
    assert!(made.is_dir(), "{} was not created", made.display());

    // And a second call over the same path adopts it rather than failing —
    // the concurrent-test-process case, which must not be a refusal.
    assert_eq!(
        override_base_by_name(&fresh).expect("an existing base is adopted"),
        fresh,
    );

    // `.` noise is dropped, so nothing downstream sees a spelling the
    // filesystem does not.
    let noisy = parent_of("noisy").join(".").join("base");
    let base = override_base_by_name(&noisy).expect("a fresh base under our own dir");
    assert_eq!(base, anchor.join("noisy").join("base"));

    // A plain file at a missing component.
    let filed = parent_of("file");
    std::fs::write(filed.join("base"), b"not a directory").expect("plant a file");
    let err =
        override_base_by_name(&filed.join("base").join("deeper")).expect_err("a file is refused");
    assert!(err.contains("is not a directory"), "{err}");

    // A symlink at a missing component — the redirection the finding is
    // about. Unix-only *as a fixture*: `std::os::windows::fs::symlink_dir`
    // needs Developer Mode or `SeCreateSymbolicLinkPrivilege`, so the shape
    // is planted where it can be, and the rule it exercises is the same one
    // `chain_entry_verdict` states for every platform.
    #[cfg(unix)]
    {
        let linked = parent_of("symlink");
        let target = anchor.join("symlink-target");
        std::fs::create_dir(&target).expect("link target");
        std::os::unix::fs::symlink(&target, linked.join("base")).expect("plant a symlink");
        let err = override_base_by_name(&linked.join("base")).expect_err("a symlink is refused");
        assert!(err.contains("is a symlink"), "{err}");
        // The part that matters: nothing was created at the far end of it.
        let err = override_base_by_name(&linked.join("base").join("deeper"))
            .expect_err("a symlinked ancestor is refused too");
        assert!(err.contains("is a symlink"), "{err}");
        assert_eq!(
            std::fs::read_dir(&target)
                .expect("read the link target")
                .count(),
            0,
            "the symlink was followed and written through",
        );
    }

    // The residual, asserted so it cannot rot into an assumed guarantee: a
    // plain pre-existing directory is adopted. On Unix that is safe because
    // the arm above judges ownership and mode; here there is nothing to
    // judge it by, and #163/#164 is what closes it.
    let planted = parent_of("planted").join("base");
    std::fs::create_dir(&planted).expect("plant a directory");
    assert_eq!(
        override_base_by_name(&planted).expect("a plain directory is adopted"),
        planted,
        "adoption of a plain directory is the documented residual",
    );
}

/// The race branch on its own. The walk above stats before it creates, so a
/// pre-planted entry is caught by the stat; the branch that decides the
/// actual race is the one where `create_dir` comes back `AlreadyExists`
/// because the entry appeared *after* that stat.
///
/// [`create_or_refuse_component`] is where that lands, and planting the entry
/// before calling it reproduces exactly the state losing the race leaves
/// behind. `AlreadyExists` must not be success — which is what
/// `create_dir_all` treated it as, and is the whole of Greptile's finding.
#[test]
fn a_by_name_component_that_appears_before_creation_is_judged_not_adopted() {
    let guard = race_safe_tempdir();
    let anchor = guard
        .path()
        .canonicalize()
        .expect("resolve the scratch anchor");
    // Each shape needs its own parent, since they all plant the same name.
    let parent_of = |kind: &str| -> PathBuf {
        let dir = anchor.join(kind);
        std::fs::create_dir(&dir).expect("stand-in parent");
        dir
    };

    // Nothing there: created, and accepted.
    let made = parent_of("fresh").join("base");
    create_or_refuse_component(&made).expect("a fresh component is created");
    assert!(made.is_dir(), "{} was not created", made.display());

    // A plain file at the name.
    let filed = parent_of("file").join("base");
    std::fs::write(&filed, b"not a directory").expect("plant a file");
    let err = create_or_refuse_component(&filed).expect_err("a file is refused");
    assert!(err.contains("is not a directory"), "{err}");

    // A symlink at the name — Unix-only as a *fixture* (Windows needs
    // Developer Mode to create one), same rule either way.
    #[cfg(unix)]
    {
        let linked = parent_of("symlink").join("base");
        let target = anchor.join("symlink-target");
        std::fs::create_dir(&target).expect("link target");
        std::os::unix::fs::symlink(&target, &linked).expect("plant a symlink");
        let err = create_or_refuse_component(&linked).expect_err("a symlink is refused");
        assert!(err.contains("is a symlink"), "{err}");
        assert_eq!(
            std::fs::read_dir(&target)
                .expect("read the link target")
                .count(),
            0,
            "the symlink was followed and written through",
        );
    }

    // And the residual once more, at the level that decides it: a plain
    // directory somebody else could have planted is adopted, because `std`
    // exposes no owner on Windows to tell it from one of ours.
    let planted = parent_of("planted").join("base");
    std::fs::create_dir(&planted).expect("plant a directory");
    create_or_refuse_component(&planted).expect("a plain directory is adopted");
}

/// The rule the by-name walk judges every component against, as a pure
/// function — which is the only way the **reparse-point** case can be
/// pinned at all: junctions, cloud-file placeholders and `AppExecLink`
/// entries exist only on Windows, and `FileType::is_symlink` covers just the
/// first two of the three. A redirection the harness would write agent
/// credentials through must be refused whichever tag it carries.
#[test]
fn a_chain_entry_verdict_refuses_every_kind_of_redirection() {
    let path = Path::new("/anchor/base");
    let named = |verdict: Option<String>| verdict.unwrap_or_default();

    // A plain directory is the one shape that passes.
    assert_eq!(chain_entry_verdict(path, false, false, true), None);

    // A symlink or junction — what `std` classifies as a link.
    assert!(named(chain_entry_verdict(path, true, true, true)).contains("is a symlink"));
    // A reparse point `std` does not classify as a link. This is the case
    // `is_symlink` alone misses.
    assert!(
        named(chain_entry_verdict(path, false, true, true)).contains("is a reparse point"),
        "an unclassified reparse point must still be refused",
    );
    // A file, or anything else that is not a directory.
    assert!(named(chain_entry_verdict(path, false, false, false)).contains("is not a directory"));

    // Every refusal names the path, since the message is all the operator
    // gets — `refused_override_message` wraps it verbatim.
    for (link, reparse, dir) in [
        (true, true, true),
        (false, true, true),
        (false, false, false),
    ] {
        let why = named(chain_entry_verdict(path, link, reparse, dir));
        assert!(why.contains("/anchor/base"), "{why}");
    }
}

/// The by-name walk applies the same shape rule as the descriptor walk —
/// a relative value or one with `..` never reaches the filesystem at all.
#[test]
fn a_by_name_override_base_refuses_the_same_shapes() {
    let err = override_base_by_name(Path::new("scratch/e2e")).expect_err("relative is refused");
    assert!(err.contains("is not an absolute path"), "{err}");
    #[cfg(unix)]
    {
        let err =
            override_base_by_name(Path::new("/var/tmp/../../etc")).expect_err("`..` is refused");
        assert!(err.contains("contains a `..` component"), "{err}");
    }
}

/// The `dad-tests-<pid>-*` name is load-bearing: `cargo xtask
/// clean-e2e-tmp` reaps by that prefix and issue #461 reaps by the PID
/// inside it. Moving the root off `/tmp` must not disturb either.
#[test]
fn the_harness_root_keeps_its_pid_tagged_name() {
    let root = harness_temp_root();
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .expect("root has a UTF-8 name");
    let prefix = format!("dad-tests-{}-", std::process::id());
    assert!(
        name.starts_with(&prefix),
        "{name} does not start with {prefix}"
    );
    assert!(
        name.len() > prefix.len(),
        "{name} has no random suffix after {prefix}",
    );
    assert_eq!(
        root.parent(),
        Some(harness_temp_base().path.as_path()),
        "root must sit directly in the resolved temp base",
    );
}

/// The root is created 0o700 by `mkdir(2)` itself, not chmod'ed afterwards.
/// On a shared base the gap between the two is long enough for a local user
/// to enter a default-0o755 root and plant fixed descendants.
#[cfg(unix)]
#[test]
fn the_harness_root_is_owner_only_from_the_moment_it_exists() {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let root = harness_temp_root();
    let meta = std::fs::symlink_metadata(root).expect("stat harness root");
    let mode = meta.permissions().mode() & 0o7777;
    assert_eq!(
        private_dir_objection(meta.uid(), mode, effective_uid()),
        None,
        "{} is uid {} mode 0o{mode:o}",
        root.display(),
        meta.uid(),
    );
}

/// The exact mode the harness *claims* — `0o700`, read back off the disk —
/// for the root and, when the private `/var/tmp` rung is the one in use,
/// for its parent.
///
/// [`the_harness_root_is_owner_only_from_the_moment_it_exists`] asserts the
/// weaker `mode & 0o077 == 0`, which `0o500` and `0o000` also satisfy. This
/// pins the value the audit note and `docs/develop/e2e-temp-dirs.md` both
/// state, and it reads `symlink_metadata` rather than trusting that a
/// permissions builder was called: leftover roots turn up at `0o775`
/// because an orphaned agent re-created the path after the exit sweep
/// removed the real one, and only an on-disk assertion distinguishes a
/// harness root from a re-creation.
#[cfg(unix)]
#[test]
fn the_harness_root_and_its_private_parent_are_exactly_0o700_on_disk() {
    use std::os::unix::fs::PermissionsExt;
    let on_disk = |p: &Path| -> u32 {
        std::fs::symlink_metadata(p)
            .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
            .permissions()
            .mode()
            & 0o7777
    };

    let root = harness_temp_root();
    let root_mode = on_disk(root);
    assert_eq!(
        root_mode,
        0o700,
        "{} is 0o{root_mode:o} on disk, not the 0o700 the harness claims",
        root.display(),
    );

    // Only the `/var/tmp/dad-e2e-<uid>` rung is the harness's to hold at
    // 0o700. The system-temp fallback is 1777 by design, and a
    // `DAD_E2E_TMPDIR` base is the caller's directory, not ours.
    let private = match private_temp_parent() {
        Ok(p) => p,
        Err(why) => {
            eprintln!("skipping the parent half: no private parent here ({why})");
            return;
        }
    };
    if root.parent() != Some(private.as_path()) {
        eprintln!(
            "skipping the parent half: the base in use is not the private parent {}",
            private.display(),
        );
        return;
    }
    let parent_mode = on_disk(&private);
    assert_eq!(
        parent_mode,
        0o700,
        "{} is 0o{parent_mode:o} on disk, not the 0o700 the harness claims",
        private.display(),
    );
}

/// The per-test dir construction from `TuiDeck::try_launch_inner` — a bare
/// `tempfile::Builder` with `.permissions(0o700)` and the default `.tmp`
/// prefix — lands 0o700 on disk, verified against a control that proves the
/// umask alone would not have produced it.
///
/// `try_launch_inner`'s own assertion only runs inside a live launch, i.e.
/// under `--features e2e`. This exercises the same two lines in the fast
/// tier, so a `tempfile` upgrade that stopped honouring `permissions()` for
/// the `tempdir_in` path is named here instead of surfacing as a panic deep
/// inside a PTY test.
///
/// The control comes first because the assertion is only meaningful under a
/// permissive umask: with `umask 077` a directory created with no explicit
/// permissions at all is already owner-only, and asserting 0o700 would pass
/// while proving nothing. Skipping then is honest; asserting is not.
#[cfg(unix)]
#[test]
fn the_per_test_tempdir_is_0o700_even_when_the_umask_alone_would_not_be() {
    use std::os::unix::fs::PermissionsExt;
    let on_disk = |p: &Path| -> u32 {
        std::fs::symlink_metadata(p)
            .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
            .permissions()
            .mode()
            & 0o7777
    };
    let root = harness_temp_root();

    let control = tempfile::Builder::new()
        .prefix("umask-control-")
        .tempdir_in(root)
        .expect("umask control dir");
    let control_mode = on_disk(control.path());
    if control_mode & 0o077 == 0 {
        eprintln!(
            "skipping: the umask here already yields 0o{control_mode:o} \
             without asking, so the 0o700 below would prove nothing"
        );
        return;
    }

    let dir = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(root)
        .expect("per-test dir");
    let mode = on_disk(dir.path());
    assert_eq!(
        mode,
        0o700,
        "{} is 0o{mode:o} on disk while a dir created the same way without \
         `permissions()` is 0o{control_mode:o} — `tempfile` stopped applying \
         the mode at creation",
        dir.path().display(),
    );
}

/// The harness root must never be a descendant of the real checkout. A
/// seeded fixture that sits inside this repository is one `..` away from
/// `CLAUDE.md`, `AGENTS.md`, `.claude/` and `.agents/` — and real agents
/// walk ancestors, with the Codex worker taking such a directory as its
/// writable workspace. Skipped when `DAD_E2E_TMPDIR` is set, since pointing
/// it into the repo is an explicit (and documented) choice.
#[test]
fn the_harness_root_is_never_inside_the_repository() {
    if std::env::var_os(TEMP_BASE_ENV).is_some_and(|v| !v.is_empty()) {
        eprintln!("skipping: {TEMP_BASE_ENV} is set, so placement is explicit");
        return;
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = harness_temp_root();
    assert!(
        !root.starts_with(repo),
        "{} is inside the checkout at {}",
        root.display(),
        repo.display(),
    );
}

/// Marks the first allocation on stdout so the re-run below can capture it.
const FIRST_ALLOC_MARKER: &str = "harness-first-alloc=";

/// The `tempfile` process-global redirect, asserted for exactly what it is:
/// **defence in depth**, in force only once the harness root exists.
///
/// The root is resolved first here on purpose — that is the precondition of
/// the claim, not an accident of test order — so this covers allocations the
/// suite does not make itself (a dependency's, a call site that slipped past
/// `linkage-check` rule 8) *after* something has asked the harness for a
/// directory. It says nothing about the first allocation of a process; that
/// is the test below, and conflating the two is what let issue #322's
/// biggest allocations keep landing on the tmpfs while a green test claimed
/// otherwise.
#[test]
fn the_tempfile_redirect_catches_a_bare_constructor_once_the_root_exists() {
    let root = harness_temp_root();
    // The bare constructor IS the thing under test here, so rule 8 is opted
    // out of on this one line: linkage-check:allow-bare-tempdir
    let stray = tempfile::tempdir().expect("bare tempdir"); // linkage-check:allow-bare-tempdir
    assert!(
        stray.path().starts_with(root),
        "{} escaped the harness root {}",
        stray.path().display(),
        root.display(),
    );
}

/// Issue #322: the suite's temp-dir constructor must contain its result
/// **whatever order a test does things in** — including when it is the very
/// first thing the process does.
///
/// The ordering is the entire point, and asserting it the other way round
/// proves nothing. The predecessor of this test resolved the root and only
/// *then* allocated, so it exercised the one ordering that could not fail.
/// Reversed, and measured on `a0b616c`, the allocation went to
/// `/tmp/.tmpz5pszS` while the root was
/// `/var/tmp/dad-e2e-1000/dad-tests-1715819-eACfgW` — in all 13 fast-tier
/// binaries — because the redirect above is installed at the END of the lazy
/// initialiser and nothing had triggered it yet.
///
/// [`harness_tempdir`] is ordering-independent by construction: it resolves
/// the root before it allocates. The call is kept as the *first statement*
/// deliberately — anything above it re-introduces the favourable ordering.
///
/// Doubles as the child of
/// [`the_first_allocation_in_a_fresh_process_is_contained`], which re-runs
/// this test in its own process and reads both paths off the markers.
#[test]
fn a_harness_tempdir_lands_under_the_harness_root() {
    let stray = harness_tempdir().expect("first allocation of the process");
    let root = harness_temp_root();
    println!("{FIRST_ALLOC_MARKER}{}", stray.path().display());
    println!("{ROOT_MARKER}{}", root.display());
    assert!(
        stray.path().starts_with(root),
        "{} escaped the harness root {}",
        stray.path().display(),
        root.display(),
    );
}

/// The same claim in a genuinely fresh process, because only nextest
/// guarantees one process per test. Under plain `cargo test` some earlier
/// test in the same binary has already built the root, and the ordering the
/// test above exists to pin is silently no longer under test.
///
/// Re-runs *this* binary against that single test and reads both paths off
/// its stdout, so containment is asserted against a process whose first
/// allocation provably is the one under test.
#[test]
fn the_first_allocation_in_a_fresh_process_is_contained() {
    let exe = std::env::current_exe().expect("current exe");
    // libtest test names omit the crate segment `module_path!()` carries.
    let module = module_path!()
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| module_path!());
    let child_test = format!("{module}::a_harness_tempdir_lands_under_the_harness_root");
    let out = std::process::Command::new(&exe)
        .arg(&child_test)
        .args(["--exact", "--test-threads=1", "--nocapture"])
        .output()
        .expect("re-run this test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "child run of {child_test} failed: {}\n{stdout}{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    // `--nocapture` interleaves markers onto libtest's own `test <name> ...`
    // line, so match anywhere in the line rather than at the start.
    let field =
        |marker: &str| -> String {
            stdout
            .lines()
            .find_map(|l| l.split_once(marker).map(|(_, rest)| rest.trim().to_string()))
            .unwrap_or_else(|| {
                panic!(
                    "child never reported {marker} — did `{child_test}` match no tests?\n{stdout}"
                )
            })
        };
    let first = field(FIRST_ALLOC_MARKER);
    let root = field(ROOT_MARKER);
    assert!(
        Path::new(&first).starts_with(&root),
        "the first allocation of a fresh process escaped the harness root:\n  \
         allocated {first}\n  root      {root}",
    );
}

/// Build a directory whose absolute path is *exactly* `len` bytes.
///
/// Deliberately not under the harness root: the point is to control the
/// total length, and the harness root's own length is whatever this machine
/// makes it. The returned guard removes it. `None` when every short anchor
/// on this machine is already longer than `len`, in which case the caller
/// skips rather than asserting something it did not actually build.
#[cfg(unix)]
fn padded_base_of_len(len: usize) -> Option<tempfile::TempDir> {
    // `tempfile` always appends exactly six random characters.
    const RAND: usize = 6;
    let anchor = [PathBuf::from(SHARED_VAR_TMP), std::env::temp_dir()]
        .into_iter()
        .filter(|p| p.is_dir())
        .find(|p| p.as_os_str().len() + 1 + RAND <= len)?;
    let pad = len - anchor.as_os_str().len() - 1 - RAND;
    tempfile::Builder::new()
        .prefix(&"p".repeat(pad))
        .tempdir_in(&anchor)
        .ok()
}

/// Reproduce the deepest path the harness ever *binds*, using the same
/// constructors it uses, with the worst-case PID width baked in (Linux's
/// default `pid_max` is 4194304 — seven digits). The two `TempDir` guards
/// must be held by the caller: dropping them removes the socket's parents.
#[cfg(unix)]
fn worst_case_socket_path(base: &Path) -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let root = tempfile::Builder::new()
        .prefix("dad-tests-4194304-")
        .tempdir_in(base)
        .expect("worst-case harness root");
    // Default `tempfile` prefix — what `race_safe_tempdir` and a bare
    // `harness_tempdir()` both produce.
    let inner = tempfile::Builder::new()
        .tempdir_in(root.path())
        .expect("worst-case per-test dir");
    let sock = inner.path().join("attach.sock");
    (root, inner, sock)
}

/// The boundary the veto actually claims: a base at exactly the maximum
/// accepted length composes to exactly `SUN_PATH_USABLE` bytes and binds.
///
/// The equality is the part that matters — it is what makes
/// `HARNESS_SOCKET_OVERHEAD` unable to drift. If `tempfile`'s suffix grew,
/// or a longer socket name appeared, or the constant were trimmed, the
/// composed path would stop matching the budget and this fails.
#[cfg(unix)]
#[test]
fn socket_budget_binds_at_exactly_the_maximum_base_length() {
    let Some(base) = padded_base_of_len(MAX_TEMP_BASE_LEN) else {
        eprintln!("skipping: no anchor short enough for a {MAX_TEMP_BASE_LEN}-byte base");
        return;
    };
    assert!(
        fits_socket_budget(base.path()),
        "{} ({} bytes) should be exactly at the limit",
        base.path().display(),
        base.path().as_os_str().len(),
    );
    let (_root, _inner, sock) = worst_case_socket_path(base.path());
    assert_eq!(
        sock.as_os_str().len(),
        SUN_PATH_USABLE,
        "composed {} — the real nesting no longer matches \
         HARNESS_SOCKET_OVERHEAD ({HARNESS_SOCKET_OVERHEAD})",
        sock.display(),
    );
    let listener = std::os::unix::net::UnixListener::bind(&sock);
    assert!(
        listener.is_ok(),
        "cannot bind {} ({} bytes): {:?}",
        sock.display(),
        sock.as_os_str().len(),
        listener.err(),
    );
}

/// One byte over is refused by the veto, and that byte is real: the
/// composed path is one past `sun_path` on macOS/BSD, where 104 bytes is
/// the cap. Linux allows 108, so the bind itself would still succeed here —
/// the veto is calibrated to the smaller platform on purpose, which is why
/// this asserts the arithmetic rather than the syscall. That the cap is
/// real at all is proven by the test below.
#[cfg(unix)]
#[test]
fn socket_budget_refuses_one_byte_over_the_maximum_base_length() {
    let over = MAX_TEMP_BASE_LEN + 1;
    let Some(base) = padded_base_of_len(over) else {
        eprintln!("skipping: no anchor short enough for an {over}-byte base");
        return;
    };
    assert!(
        !fits_socket_budget(base.path()),
        "{} ({} bytes) should be one byte too long",
        base.path().display(),
        base.path().as_os_str().len(),
    );
    let (_root, _inner, sock) = worst_case_socket_path(base.path());
    assert_eq!(sock.as_os_str().len(), SUN_PATH_USABLE + 1);
}

/// The cap is a real syscall failure, not a convention: past the kernel's
/// `sun_path` (108 on Linux, 104 on macOS/BSD) `bind(2)` refuses outright.
/// This is the failure the ladder exists to avoid, and the reason the whole
/// budget is not simply "use the longest path you like".
#[cfg(unix)]
#[test]
fn a_socket_path_past_the_kernel_cap_cannot_be_bound() {
    // 13 past the macOS-calibrated maximum is 116 composed bytes — over the
    // cap on every platform this suite runs on.
    let far_over = MAX_TEMP_BASE_LEN + 13;
    let Some(base) = padded_base_of_len(far_over) else {
        eprintln!("skipping: no anchor short enough for a {far_over}-byte base");
        return;
    };
    let (_root, _inner, sock) = worst_case_socket_path(base.path());
    let err = std::os::unix::net::UnixListener::bind(&sock)
        .expect_err("a path past sun_path must not bind");
    eprintln!(
        "bind({} bytes) failed as expected: {err}",
        sock.as_os_str().len()
    );
}

/// The everyday case, at whatever depth this machine actually produces:
/// the harness's own nesting still binds.
#[cfg(unix)]
#[test]
fn a_socket_still_binds_at_the_depth_the_harness_uses() {
    let dir = race_safe_tempdir();
    let sock = dir.path().join("attach.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock);
    assert!(
        listener.is_ok(),
        "cannot bind {} ({} bytes): {:?}",
        sock.display(),
        sock.as_os_str().len(),
        listener.err(),
    );
}

/// Every harness tempdir nests under the one per-process root, so a killed
/// run leaves a single reapable directory instead of scattered `/tmp/.tmp*`
/// dirs that are indistinguishable from any other Rust program's.
///
/// Doubles as the child of
/// [`harness_temp_root_is_removed_when_the_process_exits_normally`], which
/// re-runs this test and reads the root off the marker line below.
#[test]
fn race_safe_tempdir_nests_under_the_harness_root() {
    let dir = race_safe_tempdir();
    assert!(
        dir.path().starts_with(harness_temp_root()),
        "{} is not under the harness root {}",
        dir.path().display(),
        harness_temp_root().display(),
    );
    println!("{ROOT_MARKER}{}", harness_temp_root().display());
}

/// The lock dir is contained by the root and hardened to 0o700 like every
/// other harness dir. It previously used a bare `tempfile::Builder` with no
/// re-chmod, so the daemon's `bind_socket` umask flip left it
/// world-traversable — 474 of 521 leftovers were `drwxrwxr-x` (issue #358).
#[cfg(unix)]
#[test]
fn init_test_env_lock_dir_is_contained_and_mode_0700() {
    use std::os::unix::fs::PermissionsExt;
    init_test_env();
    let lock = lock_dir_path().expect("init_test_env creates the lock dir");
    assert!(
        lock.starts_with(harness_temp_root()),
        "lock dir {} escaped the harness root",
        lock.display(),
    );
    let mode = std::fs::metadata(&lock)
        .expect("stat lock dir")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "lock dir mode is {mode:o}, want 700");
}

/// A test process that exits normally takes its whole temp root with it.
///
/// Re-runs *this* binary against a single test that provably creates the
/// root, then asserts the child left nothing behind. This is the regression
/// guard for the original defect: the lock dir lived in a
/// `static OnceLock<TempDir>`, and because Rust never drops statics, it
/// leaked once per test process even when every test passed.
///
/// Unix-only, matching [`register_temp_root_cleanup`]: there is no `atexit`
/// binding in scope on Windows, so a Windows run leaks its root until
/// `cargo xtask clean-e2e-tmp` (which is cross-platform) is invoked. The
/// containment test above still covers Windows.
#[cfg(unix)]
#[test]
fn harness_temp_root_is_removed_when_the_process_exits_normally() {
    let exe = std::env::current_exe().expect("current exe");
    // libtest test names omit the crate segment that `module_path!()`
    // carries. Getting this wrong makes the filter match nothing, the child
    // exit 0 having run no tests, and this assertion pass vacuously — so
    // the missing marker below is treated as a failure, not a skip.
    let module = module_path!()
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| module_path!());
    let child_test = format!("{module}::race_safe_tempdir_nests_under_the_harness_root");
    let out = std::process::Command::new(&exe)
        .arg(&child_test)
        .args(["--exact", "--test-threads=1", "--nocapture"])
        .output()
        .expect("re-run this test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "child run of {child_test} failed: {}\n{stdout}{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    // `--nocapture` interleaves the marker onto libtest's own
    // `test <name> ... ` line, so match it anywhere in the line rather than
    // at the start.
    let root = stdout
        .lines()
        .find_map(|l| l.split_once(ROOT_MARKER).map(|(_, rest)| rest.trim()))
        .unwrap_or_else(|| {
            panic!(
                "child never reported a temp root — did `{child_test}` match no tests?\n{stdout}"
            )
        });
    assert!(
        !Path::new(root).exists(),
        "child exited cleanly but left its temp root behind: {root}",
    );
}

/// The whole probe arriving in one PTY read (the common case) is answered
/// with the flags reply first, then DA1 — the order crossterm expects.
#[test]
fn answer_terminal_queries_replies_to_a_single_chunk_probe() {
    let mut scan = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    answer_terminal_queries(b"\x1b[?u\x1b[c", &mut scan, &mut out);
    assert_eq!(out, b"\x1b[?1u\x1b[?62;22c".to_vec());
}

/// A probe split across two reads must still be answered exactly once —
/// the scan buffer retains just enough trailing context to complete the
/// match, and retained bytes are guaranteed match-free so nothing is
/// answered twice.
#[test]
fn answer_terminal_queries_handles_a_split_probe_without_duplicating() {
    let mut scan = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    answer_terminal_queries(b"noise\x1b[?", &mut scan, &mut out);
    assert!(out.is_empty(), "no complete query yet, got {out:?}");
    answer_terminal_queries(b"u\x1b[c more", &mut scan, &mut out);
    assert_eq!(out, b"\x1b[?1u\x1b[?62;22c".to_vec());

    // Ordinary follow-up output must not re-trigger a reply.
    let before = out.len();
    answer_terminal_queries(b"plain output\r\n", &mut scan, &mut out);
    assert_eq!(out.len(), before, "a second reply leaked: {out:?}");
}

#[test]
fn strip_jsonc_comments_drops_line_and_block_comments() {
    let input = "{\n  // line comment\n  /* block\n  comment */ \"a\": 1\n}";
    let out = strip_jsonc_comments(input);
    // serde_json must be able to parse the result without the
    // JSONC comment tokens.
    let v: serde_json::Value = serde_json::from_str(&out).expect("stripped output parses");
    assert_eq!(v["a"], serde_json::json!(1));
}

#[test]
fn strip_jsonc_comments_preserves_string_literal_slashes() {
    let input = r#"{"url": "https://example.com/path", "marker": "//keep" }"#;
    let out = strip_jsonc_comments(input);
    let v: serde_json::Value = serde_json::from_str(&out).expect("parses");
    assert_eq!(v["url"], "https://example.com/path");
    assert_eq!(v["marker"], "//keep");
}

#[test]
fn strip_hooks_from_claude_settings_jsonc_input_strips_hooks() {
    // M3.1 auditor S0 regression: a `//`-comment-bearing
    // settings.json must round-trip through the stripper with
    // its `hooks` key removed, NOT pass through unchanged.
    let raw =
        "{\n  // top-level comment\n  \"hooks\": {\"PostToolUse\": []},\n  \"theme\": \"dark\"\n}";
    let out = strip_hooks_from_claude_settings(raw).expect("jsonc parses after stripping");
    assert!(
        !out.contains("hooks"),
        "stripped settings must not still mention `hooks`: {out}"
    );
    assert!(
        out.contains("\"theme\""),
        "stripped settings must keep non-hook keys: {out}"
    );
}

#[test]
fn strip_hooks_from_claude_settings_truly_malformed_fails_closed() {
    // Garbage that isn't valid JSON even after comment stripping
    // is rejected with an Err — fail-CLOSED rather than letting
    // the host's hooks survive into the test (M3.1 auditor S0).
    let result = strip_hooks_from_claude_settings("{ this is not valid json at all");
    assert!(result.is_err());
    let err_text = result.unwrap_err().to_string();
    assert!(
        err_text.contains("not valid JSON"),
        "error must explain why the file was rejected: {err_text}"
    );
}

#[test]
fn toml_escape_passes_plain_strings_through() {
    assert_eq!(toml_escape("simple"), "simple");
    assert_eq!(toml_escape("with spaces"), "with spaces");
}

#[test]
fn toml_escape_quotes_and_backslashes_use_basic_escapes() {
    assert_eq!(toml_escape(r#"quote " inside"#), r#"quote \" inside"#);
    assert_eq!(toml_escape(r"back \ slash"), r"back \\ slash");
}

#[test]
fn toml_escape_handles_named_control_chars() {
    assert_eq!(toml_escape("line\nbreak"), r"line\nbreak");
    assert_eq!(toml_escape("tab\there"), r"tab\there");
    assert_eq!(toml_escape("cr\rback"), r"cr\rback");
    assert_eq!(toml_escape("bel\x08"), r"bel\b");
    assert_eq!(toml_escape("ff\x0c"), r"ff\f");
}

#[test]
#[allow(non_snake_case)]
fn toml_escape_emits_uXXXX_for_unnamed_control_chars() {
    // NUL, ESC, DEL.
    assert_eq!(toml_escape("\0"), "\\u0000");
    assert_eq!(toml_escape("\x1b"), "\\u001B");
    assert_eq!(toml_escape("\x7f"), "\\u007F");
}

#[test]
fn match_needles_in_order_finds_full_sequence_when_ordered() {
    // M4.6 P1: rolling-history matcher must succeed when every
    // needle appears in order, even when two transitions land
    // back-to-back in a single chunk.
    let haystack = b"prelude Thinking... then Working with `Bash` then Idle now";
    let needles = ["Thinking", "Working", "Bash", "Idle"];
    let matched = match_needles_in_order(haystack, &needles);
    assert_eq!(matched, needles.len());
}

#[test]
fn match_needles_in_order_stops_when_needle_is_out_of_order() {
    // Sequence: text contains Working before Thinking — the
    // matcher must stop at index 1 (Thinking found, Working
    // already passed by the cursor).
    let haystack = b"Working appears first, then Thinking arrives later";
    let needles = ["Thinking", "Working"];
    let matched = match_needles_in_order(haystack, &needles);
    // Thinking is found (offset > 0). Then we search for Working
    // AFTER Thinking — and there's no second Working, so the
    // match stops at 1.
    assert_eq!(matched, 1);
}

#[test]
fn match_needles_in_order_returns_zero_when_first_needle_missing() {
    // Used by wait_for_strings_in_order's timeout path: if even
    // the first needle never appears, `matched` stays 0 so the
    // panic message points at the right substring.
    let haystack = b"completely unrelated output, no status labels here";
    let needles = ["Thinking", "Working"];
    let matched = match_needles_in_order(haystack, &needles);
    assert_eq!(matched, 0);
}

#[test]
fn match_needles_in_order_partial_when_later_needle_missing() {
    // Thinking + Working land in the history, but Bash never
    // shows up — matcher reports 2 (the cursor advanced past
    // both before failing on Bash). wait_for_strings_in_order
    // then surfaces "did not see `Bash` (needle #3 of 4)" on
    // timeout.
    let haystack = b"Thinking happened then Working took over, no tool was used";
    let needles = ["Thinking", "Working", "Bash", "Idle"];
    let matched = match_needles_in_order(haystack, &needles);
    assert_eq!(matched, 2);
}

#[test]
fn match_prefix_then_terminal_accepts_idle_after_prefix() {
    // prd-77 chain-smoke: full prefix in order, then a rendered
    // Idle — the classic happy path. Terminal is satisfied.
    let haystack = b"Thinking... Working with `Bash` then Idle now";
    let (matched, terminal) =
        match_prefix_then_terminal(haystack, &["Thinking", "Working", "Bash"], &["Idle"]);
    assert_eq!(matched, 3);
    assert!(terminal);
}

#[test]
fn match_prefix_then_terminal_accepts_placeholder_when_idle_absent() {
    // print-mode lifecycle: the agent exits before any Idle
    // frame, so the pane falls back to the placeholder. The
    // placeholder alternative (seen AFTER Bash) satisfies the
    // terminal even though `Idle` never appears.
    let haystack = b"Thinking... Working with `Bash`, agent exited, Launch an agent to get started";
    let (matched, terminal) = match_prefix_then_terminal(
        haystack,
        &["Thinking", "Working", "Bash"],
        &["Idle", "Launch an agent to get started"],
    );
    assert_eq!(matched, 3);
    assert!(terminal);
}

#[test]
fn match_prefix_then_terminal_ignores_terminal_before_prefix_completes() {
    // A restored session renders its default `Idle` (and may show
    // the placeholder) BEFORE the agent starts. That stale early
    // terminal must NOT count: searching only after the prefix
    // cursor means a pre-lifecycle Idle is rejected.
    let haystack = b"Idle (restored) then Thinking... Working with `Bash`, nothing after";
    let (matched, terminal) = match_prefix_then_terminal(
        haystack,
        &["Thinking", "Working", "Bash"],
        &["Idle", "Launch an agent to get started"],
    );
    assert_eq!(matched, 3);
    assert!(!terminal);
}

/// The observed `claude_001_thinking_working_idle` flake, reduced to its
/// mechanism. The pane's placeholder was painted BEFORE the working
/// lifecycle and never repainted after it, so the post-prefix byte stream
/// carries none of its bytes — yet the user is plainly looking at it, and
/// the failing run's own panic message printed a final grid containing it.
///
/// The byte stream is not a faithful record of what is on screen: ratatui
/// renders DIFFERENTIALLY (an unchanged cell region emits nothing at all)
/// and can split one visible line across several writes when styling
/// changes mid-line. Either is enough to hide a terminal state that has
/// genuinely been reached, which is why the grid is consulted as a second
/// source of evidence.
#[test]
fn terminal_reached_accepts_a_terminal_state_that_arrives_on_the_grid() {
    // Placeholder bytes appear only BEFORE the prefix completes, so the
    // post-cursor stream never carries them.
    let haystack = b"Thinking... Working `Bash`";
    let working = "1 claude-sm... - Bash\nDir: .tmpZNOTi9\nWorking";
    let exited = "1 No agent - claude-sm...\nDir: .tmpZNOTi9\nLaunch an agent to get started";
    let prefix = ["Thinking", "Working", "Bash"];
    let terminals = ["Idle", "Launch an agent to get started"];
    let mut baseline = None;

    // First poll past the gate only latches what was already on screen.
    let (matched, terminal) =
        terminal_reached(haystack, &prefix, &terminals, &mut baseline, || {
            working.to_string()
        });
    assert_eq!(matched, 3);
    assert!(!terminal, "nothing has arrived yet on the first poll");

    // The agent exits and the card repaints to the placeholder.
    let (_, terminal) = terminal_reached(haystack, &prefix, &terminals, &mut baseline, || {
        exited.to_string()
    });
    assert!(
        terminal,
        "a terminal state that ARRIVES on screen must satisfy the terminal \
         condition even when a differential render never re-emitted its bytes \
         after the prefix"
    );
}

/// Greptile P2 on #585, and a false pass that `delegate_014` would have
/// hit every run. Its worker command is
/// `claude --model … --allowedTools Bash Read Write`, the deck renders a
/// role's command on its card, and that test's terminal alternatives are
/// `["Bash", "bash"]` — so the needle sits on the grid from boot. A bare
/// "is it on screen" check would pass the instant the prefix completed,
/// without the worker ever running a Bash tool. Only a needle that ARRIVES
/// after the prefix counts.
#[test]
fn terminal_reached_rejects_a_needle_that_was_on_the_grid_all_along() {
    let haystack = b"Thinking... Working, worker still going";
    let grid = "orchestrator | worker\n\
                command: claude --model haiku --allowedTools Bash Read Write\n\
                Working";
    let mut baseline = None;
    let prefix = ["Thinking", "Working"];
    let terminals = ["Bash", "bash"];

    for _ in 0..3 {
        let (matched, terminal) =
            terminal_reached(haystack, &prefix, &terminals, &mut baseline, || {
                grid.to_string()
            });
        assert_eq!(matched, 2);
        assert!(
            !terminal,
            "a needle that has been on screen since boot must never satisfy \
             the terminal condition — the test would finish while the worker \
             is still working"
        );
    }
}

/// The ordering guarantee must survive the grid arm: a restored session
/// renders a default `Idle` before the agent starts, and that must still
/// not count. The grid is consulted ONLY once the prefix has fully
/// matched, so the gate is shut while the stale state is on screen.
#[test]
fn terminal_reached_still_ignores_a_terminal_state_before_the_prefix_completes() {
    let haystack = b"Idle (restored) then Thinking... Working took over, no tool used";
    let mut baseline = None;
    let (matched, terminal) = terminal_reached(
        haystack,
        &["Thinking", "Working", "Bash"],
        &["Idle", "Launch an agent to get started"],
        &mut baseline,
        || "Idle".to_string(),
    );
    assert_eq!(matched, 2);
    assert!(
        !terminal,
        "an Idle on screen while the working lifecycle is still incomplete \
         must never satisfy the terminal condition — the prefix gate is what \
         makes the grid arm safe"
    );
    assert!(
        baseline.is_none(),
        "the baseline must not latch before the gate opens, or it would \
         capture a mid-lifecycle screen"
    );
}

/// The cheap path stays cheap: when the byte stream already answers the
/// question, the grid is never rendered. `snapshot_grid` locks and walks
/// the whole screen, and this decision is polled every 20 ms.
#[test]
fn terminal_reached_does_not_render_the_grid_when_the_stream_settles_it() {
    let mut rendered = false;
    let mut baseline = None;
    let (matched, terminal) = terminal_reached(
        b"Thinking... Working `Bash` then Idle",
        &["Thinking", "Working", "Bash"],
        &["Idle"],
        &mut baseline,
        || {
            rendered = true;
            String::new()
        },
    );
    assert_eq!(matched, 3);
    assert!(terminal);
    assert!(
        !rendered,
        "the grid must not be rendered when the byte stream already matched"
    );
}

#[test]
fn match_prefix_then_terminal_reports_incomplete_prefix() {
    // Bash never shows up: prefix stalls at 2 and terminal is
    // forced false, so the timeout path points at the missing
    // prefix needle rather than the terminal alternatives.
    let haystack = b"Thinking happened then Working took over, then Idle, no tool used";
    let (matched, terminal) = match_prefix_then_terminal(
        haystack,
        &["Thinking", "Working", "Bash"],
        &["Idle", "Launch an agent to get started"],
    );
    assert_eq!(matched, 2);
    assert!(!terminal);
}

/// Scenario: Import a synthetic OpenCode auth file while a hostile host config
/// sits beside it. Only auth is copied, a minimal isolated config is created,
/// and the imported token is registered for recording redaction.
#[test]
fn opencode_import_is_auth_only_and_synthesizes_minimal_config() {
    let source = race_safe_tempdir();
    let target = race_safe_tempdir();
    let source_auth = source.path().join(".local/share/opencode/auth.json");
    std::fs::create_dir_all(source_auth.parent().unwrap()).expect("source auth dir");
    std::fs::write(
        &source_auth,
        r#"{"openrouter":{"type":"api","key":"test-secret-token-249"}}"#,
    )
    .expect("source auth");
    let source_config = source.path().join(".config/opencode/opencode.jsonc");
    std::fs::create_dir_all(source_config.parent().unwrap()).expect("source config dir");
    std::fs::write(
        &source_config,
        r#"{"plugin":["host-plugin"],"mcp":{"host":{"command":"leak-secret"}}}"#,
    )
    .expect("host config");

    let redactions = import_opencode_credentials_from(source.path(), target.path())
        .expect("isolated OpenCode import");
    let imported_auth = target.path().join(".local/share/opencode/auth.json");
    assert_eq!(
        std::fs::read_to_string(imported_auth).unwrap(),
        r#"{"openrouter":{"type":"api","key":"test-secret-token-249"}}"#
    );
    assert_eq!(
        std::fs::read_to_string(target.path().join(".config/opencode/opencode.json")).unwrap(),
        MINIMAL_OPENCODE_CONFIG
    );
    assert!(
        !target
            .path()
            .join(".config/opencode/opencode.jsonc")
            .exists(),
        "the host OpenCode config must never enter the isolated HOME"
    );
    assert_eq!(redactions, vec!["test-secret-token-249"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(target.path().join(".local/share/opencode/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "imported auth mode must stay private");
    }
}

/// Scenario: Point an OpenCode data root at an external directory through a
/// symlink and attempt credential import. The importer must reject the root
/// before reading its otherwise-regular auth leaf.
#[cfg(unix)]
#[test]
fn opencode_import_rejects_a_symlinked_source_root() {
    use std::os::unix::fs::symlink;

    let source = race_safe_tempdir();
    let target = race_safe_tempdir();
    let external = race_safe_tempdir();
    std::fs::write(
        external.path().join("auth.json"),
        r#"{"openrouter":{"key":"must-not-be-imported"}}"#,
    )
    .expect("external auth");
    std::fs::create_dir_all(source.path().join(".local/share")).expect("source parents");
    symlink(external.path(), source.path().join(".local/share/opencode"))
        .expect("symlink OpenCode root");

    let error = import_opencode_credentials_from(source.path(), target.path())
        .expect_err("a symlinked OpenCode root must be refused")
        .to_string();
    assert!(
        error.contains("source directory ancestor is a symlink")
            && error.contains("~/.local/share/opencode/auth.json")
            && !error.contains(source.path().to_string_lossy().as_ref()),
        "the refusal must identify the redacted source without exposing HOME: {error}"
    );
}

/// Scenario: Split a known credential across adjacent PTY recording chunks.
/// Artifact redaction must match across the chunk boundary while preserving
/// the two timestamped cast events.
#[test]
fn recording_redaction_catches_credentials_split_across_events() {
    let events = vec![
        CastEvent {
            offset_secs: 0.1,
            data: b"prefix token-".to_vec(),
        },
        CastEvent {
            offset_secs: 0.2,
            data: b"secret-249 suffix".to_vec(),
        },
    ];
    let redacted = redact_cast_events(&events, &["token-secret-249".to_string()]);
    assert_eq!(redacted.len(), events.len());
    let joined: Vec<u8> = redacted.into_iter().flatten().collect();
    assert!(
        joined
            .windows(RECORDING_CREDENTIAL_REDACTION.len())
            .any(|window| window == RECORDING_CREDENTIAL_REDACTION)
    );
    assert!(
        !joined
            .windows(b"token-secret-249".len())
            .any(|window| window == b"token-secret-249"),
        "the split credential survived recording redaction: {:?}",
        String::from_utf8_lossy(&joined)
    );
}

// -----------------------------------------------------------------------
// Issue #502/#785 — authorising a real-agent run from an API key
// -----------------------------------------------------------------------

/// Scenario: Ask for the identifier Claude Code files an API key under. It
/// is the last 20 characters, it is counted in characters rather than bytes
/// so a non-ASCII value cannot panic on a split code point, and a key
/// shorter than 20 characters comes back whole instead of being padded or
/// truncated.
#[test]
fn the_api_key_response_id_is_the_last_twenty_characters() {
    let key = "sk-ant-api03-0123456789abcdefghijklmnopqrstuvwxyz";
    let id = claude_api_key_response_id(key);
    assert_eq!(id.chars().count(), 20);
    assert!(key.ends_with(&id));
    assert_eq!(claude_api_key_response_id("short"), "short");
    // Multi-byte characters: 20 CHARS, and the result stays valid UTF-8.
    let unicode: String = std::iter::repeat_n('é', 25).collect();
    assert_eq!(claude_api_key_response_id(&unicode).chars().count(), 20);
}

/// Scenario: Register an API key for recording redaction and then render
/// both the key and the 20-character suffix Claude Code's approval prompt
/// paints on the terminal into a grid. Neither survives into the artifact.
/// The suffix half is the one that matters: a derivative is what no
/// masking layer covers even where one exists, these artifacts are local
/// files with no such layer downstream at all, and the prompt renders
/// exactly this derivative whenever the approval seeding is missing or
/// wrong.
#[test]
fn the_api_key_and_its_rendered_suffix_are_both_redacted_from_recordings() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let redactions = api_key_recording_redactions(key);
    let suffix = claude_api_key_response_id(key);

    let prompt_grid = format!(
        "Detected a custom API key in your environment\n  ANTHROPIC_API_KEY: sk-ant-...{suffix}\n  Do you want to use this API key?\n"
    );
    let redacted = redact_known_credentials_text(&prompt_grid, &redactions);
    assert!(
        !redacted.contains(&suffix),
        "the rendered key suffix survived redaction: {redacted}"
    );
    assert!(redacted.contains("[REDACTED-CREDENTIAL]"));

    let env_dump = format!("$ env | grep ANTHROPIC\nANTHROPIC_API_KEY={key}\n");
    let redacted = redact_known_credentials_bytes(env_dump.as_bytes(), &redactions);
    let redacted = String::from_utf8_lossy(&redacted);
    assert!(
        !redacted.contains(key) && !redacted.contains(suffix.as_str()),
        "the key survived redaction: {redacted}"
    );
}

// -----------------------------------------------------------------------
// PR #805 audit blocker 2 — the imported CLAUDE documents are registered
// -----------------------------------------------------------------------

/// A FABRICATED Claude Code OAuth credential document, of the real
/// `claudeAiOauth` shape. Never a real credential: every token body is
/// deterministic pseudo-random filler carrying `FAKE`, generated so that no
/// eight-character window of it occurs twice, which is what lets
/// [`assert_no_fragment_survives`] treat any surviving window as evidence
/// rather than as a coincidence.
///
/// The tokens are 128 characters on purpose. At a four-space indent the
/// value starts at column 21 of a 120-column grid, so a 128-character token
/// breaks 100 / 28 — the wrapped shape blocker A was about, reached here
/// through a `cat` of the imported file rather than through an env dump.
const FABRICATED_CLAUDE_CREDENTIALS: &str = concat!(
    "{\n",
    "  \"claudeAiOauth\": {\n",
    "    \"accessToken\": \"sk-ant-oat01-FAKE-C3J27XDCG2LmlZGEONYlgCtjfIZ4SOcMz9CPVNPkNa1Hedcm4pMbXDuCL1mHoOsFaQfDPrAJ71fTquWoGsbeKXgzg2sye9b2Rann76dEyTzAeK\",\n",
    "    \"refreshToken\": \"sk-ant-ort01-FAKE-96ipbNClShVP4wY4for9duMl7JRU7BT4dK4bLqtAml2hLH8UX98KdSuNvql9zt5X399PGjr0rQSlBdvI5cA7qGsH4AzQ76ltKxzLbtKMJIHBWR\",\n",
    "    \"expiresAt\": 1799999999000,\n",
    "    \"scopes\": [\"user:inference\", \"user:profile\"],\n",
    "    \"subscriptionType\": \"max\"\n",
    "  }\n",
    "}\n",
);

/// The `accessToken` of [`FABRICATED_CLAUDE_CREDENTIALS`], as a value the
/// assertions can name.
const FABRICATED_CLAUDE_ACCESS_TOKEN: &str = "sk-ant-oat01-FAKE-C3J27XDCG2LmlZGEONYlgCtjfIZ4SOcMz9CPVNPkNa1Hedcm4pMbXDuCL1mHoOsFaQfDPrAJ71fTquWoGsbeKXgzg2sye9b2Rann76dEyTzAeK";

/// Its `refreshToken`.
const FABRICATED_CLAUDE_REFRESH_TOKEN: &str = "sk-ant-ort01-FAKE-96ipbNClShVP4wY4for9duMl7JRU7BT4dK4bLqtAml2hLH8UX98KdSuNvql9zt5X399PGjr0rQSlBdvI5cA7qGsH4AzQ76ltKxzLbtKMJIHBWR";

/// A FABRICATED hook-stripped `settings.json`, the second document the
/// Claude import copies into the isolated HOME. The M4.6 review already
/// noted that this file "can carry the same tokens / sensitive config that
/// motivate the 0o600 mode on credentials.json"; this is that shape, plus the
/// long-but-innocuous entries a real one is mostly made of — a
/// `permissions.allow` rule and a model id — which the
/// [`CredentialScope::ConfigDocument`] width must leave alone.
const FABRICATED_CLAUDE_SETTINGS: &str = concat!(
    "{\n",
    "  \"apiKeyHelper\": \"/opt/fake-secrets/print-key.sh\",\n",
    "  \"model\": \"claude-haiku-4-5-20251001\",\n",
    "  \"permissions\": { \"allow\": [\"Bash(cargo nextest run:*)\"] },\n",
    "  \"env\": {\n",
    "    \"ANTHROPIC_AUTH_TOKEN\": \"sk-ant-atk01-FAKE-KkOo01rEP45I6HlP5N8Gu9RHECs8ROetAtHF5dVM3VRB02r7uJWRph3du4sn5eVxhPuXpGruawHQtJ\",\n",
    "    \"DATABASE_URL\": \"postgres://svc:FAKE-Xy7Qm2Vb9Lr4Ts8Wn3Hd@db.invalid:5432/app\",\n",
    "    \"NO_COLOR\": \"1\"\n",
    "  }\n",
    "}\n",
);

/// The auth token inside [`FABRICATED_CLAUDE_SETTINGS`].
const FABRICATED_CLAUDE_SETTINGS_TOKEN: &str = "sk-ant-atk01-FAKE-KkOo01rEP45I6HlP5N8Gu9RHECs8ROetAtHF5dVM3VRB02r7uJWRph3du4sn5eVxhPuXpGruawHQtJ";

/// A credential-bearing `env` entry under a name the sensitive-key list does
/// NOT match — PR #805 audit P1. A `postgres://user:password@host/db` is a
/// complete password-bearing credential and `DATABASE_URL` looks like
/// ordinary configuration, which is exactly the combination the key-name rule
/// used to copy into the isolated HOME unregistered.
const FABRICATED_CLAUDE_SETTINGS_DATABASE_URL: &str =
    "postgres://svc:FAKE-Xy7Qm2Vb9Lr4Ts8Wn3Hd@db.invalid:5432/app";

/// Scenario: Point `HOME` at a fabricated host home holding a
/// `~/.claude/.credentials.json` and a `~/.claude/settings.json`, run the
/// REAL `import_claude_credentials` against a fresh isolated HOME, and then:
/// assert it returned every credential value in those documents, assert the
/// bytes it actually wrote into the isolated HOME still hold them, assert it
/// self-registered them process-globally, and finally render the written
/// bytes onto a real 120-column vt100 grid the way a `cat` of the imported
/// file inside a pane would and push that grid through every sink the harness
/// persists or prints — `final-grid.txt`, the SVG rendered from it,
/// `full-stream.cast`, and the panic message the diagnostic seam renders. No
/// eight-character fragment of any token survives into any of them.
///
/// This is PR #805's audit blocker 2 stated as a test. The Claude importer
/// was the one of four that registered NOTHING from the documents it copies,
/// on the ground that the credential set is "never rendered". That is a
/// convention rather than a structural property — an agent command, a
/// contributor-authored test, an auth diagnostic or an accidental file dump
/// paints it onto the terminal — and it is the route most reel clips take,
/// which is the route whose `.cast` the demo reel publishes.
///
/// **It drives the importer because the first version of it did not, and the
/// second PR #805 audit caught that.** It called `claude_recording_redactions`
/// directly, so deleting either importer call site, or the importer's
/// `register_diagnostic_redactions`, left it green while claiming to be the
/// regression test for exactly those lines — the third vacuous guard in this
/// work. Every assertion above the render is now anchored on the importer's
/// own return value, the files it wrote, or the global store it registered
/// into, and each of those three sites was broken in turn and watched to fail.
#[test]
fn a_rendered_imported_claude_credential_document_survives_into_no_sink() {
    // A fabricated HOST home, so the importer's `host_home()` reads these
    // documents rather than the developer's. Both API keys are cleared: with
    // one set, `import_claude_credentials` takes its key-authorises branch
    // and `install_credential_redaction` seeds the global store with an
    // ambient value, either of which would make the assertions below depend
    // on the machine.
    // SAFETY: single-threaded test body, and nextest gives each test its own
    // process, so nothing else in this process observes the change.
    unsafe {
        std::env::remove_var(ANTHROPIC_API_KEY_ENV);
        std::env::remove_var("OPENAI_API_KEY");
    }
    let source_home = harness_tempdir().expect("fabricated host home");
    let source_claude = source_home.path().join(".claude");
    std::fs::create_dir_all(&source_claude).expect("create the fabricated ~/.claude");
    std::fs::write(
        source_claude.join(".credentials.json"),
        FABRICATED_CLAUDE_CREDENTIALS,
    )
    .expect("write the fabricated credential document");
    std::fs::write(
        source_claude.join("settings.json"),
        FABRICATED_CLAUDE_SETTINGS,
    )
    .expect("write the fabricated settings document");
    let restore_home = std::env::var_os("HOME");
    // SAFETY: as above.
    unsafe { std::env::set_var("HOME", source_home.path()) };

    let test_home = harness_tempdir().expect("isolated test HOME");
    let imported = import_claude_credentials(test_home.path());
    // SAFETY: as above. Restored before the assertions so a failure does not
    // leave the process pointing at the fixture.
    unsafe {
        match &restore_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    let redactions = imported.expect("the fabricated documents import cleanly");

    // The bytes the importer actually WROTE, which is what an agent reads and
    // what a `cat` in a pane paints. Read back rather than assumed: the
    // credential set is copied verbatim while the settings are re-serialised
    // from the parsed document, so only one of the two is byte-identical to
    // its source.
    let written_credentials =
        std::fs::read_to_string(test_home.path().join(".claude").join(".credentials.json"))
            .expect("the importer wrote the credential document into the isolated HOME");
    let written_settings =
        std::fs::read_to_string(test_home.path().join(".claude").join("settings.json"))
            .expect("the importer wrote the settings document into the isolated HOME");

    // Non-vacuity of the REGISTRATION, anchored on the importer's own return
    // value: deleting either `claude_recording_redactions` call in
    // `import_claude_credentials` fails here. Each value is checked to be in
    // the isolated HOME too, so "registered" and "present on disk" cannot
    // drift apart.
    for (what, value, written) in [
        (
            "accessToken",
            FABRICATED_CLAUDE_ACCESS_TOKEN,
            &written_credentials,
        ),
        (
            "refreshToken",
            FABRICATED_CLAUDE_REFRESH_TOKEN,
            &written_credentials,
        ),
        (
            "settings.json's ANTHROPIC_AUTH_TOKEN",
            FABRICATED_CLAUDE_SETTINGS_TOKEN,
            &written_settings,
        ),
        (
            "settings.json's DATABASE_URL",
            FABRICATED_CLAUDE_SETTINGS_DATABASE_URL,
            &written_settings,
        ),
    ] {
        assert!(
            written.contains(value),
            "{what} is not in the document the importer wrote into the \
             isolated HOME, so this fixture proves nothing:\n{written}"
        );
        assert!(
            redactions.iter().any(|r| r == value),
            "{what} was not returned by import_claude_credentials, so it is \
             copied into the isolated HOME unredactable: {redactions:?}"
        );
    }

    // Non-vacuity of the SELF-REGISTRATION: `redact_credentials_for_output`
    // reads the process-global store and nothing else, so this is green only
    // because the importer called `register_diagnostic_redactions` itself.
    // Deleting that call fails here. It is the route the tests with no
    // `TuiDeck` depend on entirely.
    let globally = redact_credentials_for_output(&format!(
        "accessToken={FABRICATED_CLAUDE_ACCESS_TOKEN} \
         refreshToken={FABRICATED_CLAUDE_REFRESH_TOKEN} \
         ANTHROPIC_AUTH_TOKEN={FABRICATED_CLAUDE_SETTINGS_TOKEN} \
         DATABASE_URL={FABRICATED_CLAUDE_SETTINGS_DATABASE_URL}"
    ));
    for (what, value) in [
        ("accessToken", FABRICATED_CLAUDE_ACCESS_TOKEN),
        ("refreshToken", FABRICATED_CLAUDE_REFRESH_TOKEN),
        (
            "settings.json's ANTHROPIC_AUTH_TOKEN",
            FABRICATED_CLAUDE_SETTINGS_TOKEN,
        ),
        (
            "settings.json's DATABASE_URL",
            FABRICATED_CLAUDE_SETTINGS_DATABASE_URL,
        ),
    ] {
        assert!(
            !globally.contains(value),
            "{what} was not self-registered by the importer, so every route \
             with no `TuiDeck` renders it raw: {globally}"
        );
    }

    // And the configuration document's long NON-secrets are left alone, or
    // a developer's `permissions.allow` rules and model id disappear out of
    // every grid they read and each one costs a scan pass over every
    // artifact. This is what `CredentialScope::ConfigDocument` buys. `1` is
    // the `env` floor's half of the same trade: registering a two-byte
    // environment value would replace those bytes everywhere they occur.
    for innocuous in [
        "Bash(cargo nextest run:*)",
        "claude-haiku-4-5-20251001",
        "1",
    ] {
        assert!(
            !redactions.iter().any(|r| r == innocuous),
            "`{innocuous}` is not a credential and must not be registered \
             from a CONFIGURATION document: {redactions:?}"
        );
    }

    // Both written documents painted into one grid, row by row the way
    // ratatui paints, so a value too long for the width breaks across rows
    // exactly as it does in a real pane.
    let dump = format!(
        "$ cat ~/.claude/.credentials.json\n{written_credentials}\n\
         $ cat ~/.claude/settings.json\n{written_settings}\n"
    );
    let rows: Vec<String> = dump
        .lines()
        .flat_map(|line| wrap_to_width(line, GRID_COLS as usize))
        .collect();
    let grid = render_like_ratatui(GRID_COLS, &rows);

    // Non-vacuity of the SHAPE: the tokens really are split across rows, so
    // a byte-exact matcher would not have caught them and this exercises the
    // wrapped path rather than the contiguous one.
    for (what, value) in [
        ("accessToken", FABRICATED_CLAUDE_ACCESS_TOKEN),
        ("refreshToken", FABRICATED_CLAUDE_REFRESH_TOKEN),
        (
            "settings.json's ANTHROPIC_AUTH_TOKEN",
            FABRICATED_CLAUDE_SETTINGS_TOKEN,
        ),
    ] {
        assert!(
            !grid.contains(value),
            "{what} is contiguous in the raw grid, so this test is not \
             reproducing the wrapped shape:\n{grid}"
        );
    }

    // Sink 1 and 2 — `final-grid.txt` and the SVG rendered from it.
    let redacted = redact_known_credentials_text(&grid, &redactions);
    let svg = render_grid_to_svg(&redacted, GRID_COLS, u16::try_from(rows.len()).unwrap());
    for (what, value) in [
        ("accessToken", FABRICATED_CLAUDE_ACCESS_TOKEN),
        ("refreshToken", FABRICATED_CLAUDE_REFRESH_TOKEN),
        (
            "settings.json's ANTHROPIC_AUTH_TOKEN",
            FABRICATED_CLAUDE_SETTINGS_TOKEN,
        ),
        (
            "settings.json's DATABASE_URL",
            FABRICATED_CLAUDE_SETTINGS_DATABASE_URL,
        ),
    ] {
        assert_no_fragment_survives(&redacted, value, &format!("final-grid.txt / {what}"));
        assert_no_fragment_survives(&svg, value, &format!("final-grid.svg / {what}"));
    }
    assert!(redacted.contains("[REDACTED-CREDENTIAL]"));
    assert!(
        redacted.contains("\"claudeAiOauth\"") && redacted.contains("\"apiKeyHelper\""),
        "redaction ate the surrounding render, so the artifact is no longer \
         diagnostic:\n{redacted}"
    );

    // Sink 3 — `full-stream.cast`, one layer below the grid, where the row
    // change is the deck's own cursor-position escape rather than a newline.
    let events = vec![
        CastEvent {
            offset_secs: 0.1,
            data: format!(
                "\x1b[1;1H    \"accessToken\": \"{}",
                &FABRICATED_CLAUDE_ACCESS_TOKEN[..100]
            )
            .into_bytes(),
        },
        CastEvent {
            offset_secs: 0.2,
            data: format!(
                "\x1b[2;1H{}\",\x1b[K",
                &FABRICATED_CLAUDE_ACCESS_TOKEN[100..]
            )
            .into_bytes(),
        },
    ];
    let joined: Vec<u8> = redact_cast_events(&events, &redactions)
        .into_iter()
        .flatten()
        .collect();
    let cast = String::from_utf8_lossy(&joined).into_owned();
    assert_no_fragment_survives(&cast, FABRICATED_CLAUDE_ACCESS_TOKEN, "full-stream.cast");
    assert!(
        cast.contains("\x1b[2;1H"),
        "the escape that separates the rows must survive, or the cast stops \
         replaying:\n{cast:?}"
    );

    // Sink 4 — the diagnostic seam, which is what carries a grid into
    // nextest's captured output, the raw JUnit report and whatever a
    // developer pastes. Nothing registers anything here: the store already
    // holds these values because `import_claude_credentials` registered them
    // itself, which the global-store assertion above proves.
    let panic_text = format_redacted_panic(
        "deck",
        "tests/common/mod.rs:1:1",
        &format!("did not see \"ready\" within 30s.\nFinal grid:\n{grid}"),
    );
    for (what, value) in [
        ("accessToken", FABRICATED_CLAUDE_ACCESS_TOKEN),
        ("refreshToken", FABRICATED_CLAUDE_REFRESH_TOKEN),
        (
            "settings.json's ANTHROPIC_AUTH_TOKEN",
            FABRICATED_CLAUDE_SETTINGS_TOKEN,
        ),
        (
            "settings.json's DATABASE_URL",
            FABRICATED_CLAUDE_SETTINGS_DATABASE_URL,
        ),
    ] {
        assert_no_fragment_survives(&panic_text, value, &format!("the panic seam / {what}"));
    }
    assert!(
        panic_text.contains("did not see \"ready\" within 30s."),
        "the panic kept none of its diagnostic shape:\n{panic_text}"
    );
}

/// Scenario: Hand the Claude redaction collector a document that is not
/// valid JSON. It refuses the import by name rather than returning "no
/// redactions", the same stance `codex_recording_redactions` and
/// `opencode_recording_redactions` take.
///
/// A credential file we cannot read is a credential file we cannot redact,
/// and that is precisely the sink. The only branch that reaches this is the
/// deliberate "copy what we have and let it fail loudly" fallback, and a run
/// there has no usable credential from any source — so failing at the import
/// names the cause instead of dying in a PTY wait.
#[test]
fn an_unparsable_claude_credential_document_is_refused_not_silently_unredacted() {
    let error = claude_recording_redactions(
        b"NOT-A-JSON-DOCUMENT",
        "~/.claude/.credentials.json",
        CredentialScope::AuthDocument,
    )
    .expect_err("an unparsable credential document must be refused");
    let message = error.to_string();
    assert!(
        message.contains("~/.claude/.credentials.json")
            && message.contains("cannot be registered for redaction"),
        "the refusal must name the file and the reason: {message}"
    );
    assert!(
        !message.contains("NOT-A-JSON-DOCUMENT"),
        "the refusal must not quote the document body back: {message}"
    );
}

/// Scenario: Hand the credential collector a `settings.json` whose `env` map
/// carries credentials under ordinary variable names — a `DATABASE_URL` with
/// user-info, an authenticated `HTTP_PROXY`, a `COOKIE`, a `PAT` — plus the
/// short settings a real one is full of. Every credential-length value under
/// `env` is registered whatever it is called, the two-byte ones are not, and
/// a long string OUTSIDE `env` under a non-sensitive name still is not.
///
/// PR #805 audit P1. The recursion keeps only the innermost key, so an `env`
/// entry used to be judged purely on what the developer named it:
/// `ANTHROPIC_AUTH_TOKEN` matched the sensitive-name list and everything else
/// was copied into the isolated HOME unregistered. A
/// `postgres://user:password@host/db` is a complete credential.
#[test]
fn a_credential_under_an_ordinary_env_variable_name_is_registered() {
    let settings = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "permissions": { "allow": ["Bash(cargo nextest run:*)"] },
        "statusLine": { "command": "printf '%s' \"$PWD\" | sed 's|/home/dev||'" },
        "env": {
            "DATABASE_URL": "postgres://svc:FAKE-Xy7Qm2Vb9Lr4@db.invalid:5432/app",
            "HTTP_PROXY": "http://dev:FAKE-Pw9Kt3Zc7Qm@proxy.invalid:3128",
            "NPM_CONFIG__AUTH": "FAKE-bnBtLXRva2VuLXZhbHVlLWhlcmU=",
            "COOKIE": "session=FAKE-9d41c0b2e7f84a1c6b3d",
            "PAT": "FAKE-ghp-4KqR7wZ2mN8xT1vB6yH3jL5s",
            "NO_COLOR": "1",
            "TERM": "xterm",
        },
    });
    let mut values = Vec::new();
    collect_credential_values(
        &settings,
        None,
        CredentialScope::ConfigDocument,
        false,
        &mut values,
    );

    for (what, value) in [
        (
            "DATABASE_URL",
            "postgres://svc:FAKE-Xy7Qm2Vb9Lr4@db.invalid:5432/app",
        ),
        (
            "HTTP_PROXY",
            "http://dev:FAKE-Pw9Kt3Zc7Qm@proxy.invalid:3128",
        ),
        ("NPM_CONFIG__AUTH", "FAKE-bnBtLXRva2VuLXZhbHVlLWhlcmU="),
        ("COOKIE", "session=FAKE-9d41c0b2e7f84a1c6b3d"),
        ("PAT", "FAKE-ghp-4KqR7wZ2mN8xT1vB6yH3jL5s"),
    ] {
        assert!(
            values.iter().any(|v| v == value),
            "an `env` entry named {what} holds a credential and was not \
             registered, so it is copied into the isolated HOME and rendered \
             raw: {values:?}"
        );
    }

    // The floor, which is what keeps this from wrecking every artifact: a
    // registered two-byte value would replace those bytes wherever they
    // occur.
    for short in ["1", "xterm"] {
        assert!(
            !values.iter().any(|v| v == short),
            "`{short}` is a setting, not a credential, and registering it \
             would gouge those bytes out of every artifact: {values:?}"
        );
    }

    // And OUTSIDE `env` the sensitive-key rule still governs, or a real
    // settings.json's long non-secrets disappear out of every grid.
    for innocuous in [
        "claude-haiku-4-5-20251001",
        "Bash(cargo nextest run:*)",
        "printf '%s' \"$PWD\" | sed 's|/home/dev||'",
    ] {
        assert!(
            !values.iter().any(|v| v == innocuous),
            "`{innocuous}` is not under `env` and not under a sensitive key, \
             so a CONFIGURATION document must leave it alone: {values:?}"
        );
    }
}

/// Scenario: Point `HOME` at a fabricated host home whose `~/.claude.json`
/// carries an `oauthAccount`, run the real `seed_claude_project_trust`
/// against a fresh isolated HOME, and check the three things that matter: the
/// identity fields it copied come back for the recordings to redact, they are
/// registered process-globally for the diagnostics, and the descriptive
/// neighbours (`organizationRole`, `seatTier`) and a too-short display name
/// are left alone.
///
/// PR #805 audit P2. The classification stands — a uuid, an email and an
/// organisation name identify rather than authenticate — but this file is
/// copied into every real-agent test's HOME, an interactive `claude` paints
/// the account line into the pane, and that pane is what the demo reel
/// publishes. Three field names is cheaper than accepting that.
#[test]
fn the_seeded_claude_json_registers_its_account_identity_fields() {
    // SAFETY: single-threaded test body in its own nextest process.
    unsafe {
        std::env::remove_var(ANTHROPIC_API_KEY_ENV);
        std::env::remove_var("OPENAI_API_KEY");
    }
    let uuid = "6f1c9e02-FAKE-4b77-9d31-2a8e5c40b913";
    let email = "fabricated.developer@example.invalid";
    let organisation = "Fabricated Example Organisation";
    let source_home = harness_tempdir().expect("fabricated host home");
    std::fs::write(
        source_home.path().join(".claude.json"),
        serde_json::to_vec(&serde_json::json!({
            "hasCompletedOnboarding": true,
            "oauthAccount": {
                "accountUuid": uuid,
                "emailAddress": email,
                "organizationName": organisation,
                "organizationRole": "admin",
                "seatTier": "max",
                "displayName": "Ab",
            },
        }))
        .expect("serialize the fabricated ~/.claude.json"),
    )
    .expect("write the fabricated ~/.claude.json");
    let restore_home = std::env::var_os("HOME");
    // SAFETY: as above.
    unsafe { std::env::set_var("HOME", source_home.path()) };

    let test_home = harness_tempdir().expect("isolated test HOME");
    let seeded = seed_claude_project_trust(
        test_home.path(),
        &[test_home.path().to_string_lossy().into_owned()],
    );
    // SAFETY: as above.
    unsafe {
        match &restore_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
    let identity = seeded.expect("the fabricated ~/.claude.json seeds cleanly");

    let written = std::fs::read_to_string(test_home.path().join(".claude.json"))
        .expect("the seeding wrote a ~/.claude.json into the isolated HOME");
    for (what, value) in [
        ("accountUuid", uuid),
        ("emailAddress", email),
        ("organizationName", organisation),
    ] {
        assert!(
            written.contains(value),
            "{what} is not in the document the seeding wrote, so this fixture \
             proves nothing:\n{written}"
        );
        assert!(
            identity.iter().any(|v| v == value),
            "{what} was not returned for redaction, so it reaches the cast \
             the demo reel publishes: {identity:?}"
        );
    }

    // Self-registration, for the routes with no `TuiDeck` and for a caller
    // that seeds AFTER the launch (`seed_claude_trust_in_home`).
    let globally =
        redact_credentials_for_output(&format!("account {uuid} <{email}> at {organisation}"));
    for (what, value) in [
        ("accountUuid", uuid),
        ("emailAddress", email),
        ("organizationName", organisation),
    ] {
        assert!(
            !globally.contains(value),
            "{what} was not registered process-globally: {globally}"
        );
    }

    // Descriptive neighbours are not identity, and registering one would
    // replace `admin` or `max` wherever they occurred. `Ab` is the floor:
    // below eight bytes a value cannot be told apart from ordinary rendered
    // text, which is what `MIN_WRAP_FRAGMENT` means everywhere else here.
    for left_alone in ["admin", "max", "Ab"] {
        assert!(
            !identity.iter().any(|v| v == left_alone),
            "`{left_alone}` must not be registered — it is not identity, or \
             it is too short to tell apart from rendered text: {identity:?}"
        );
    }
}

// -----------------------------------------------------------------------
// Issue #502/#785 blocker A — a credential the TERMINAL WRAPPED
// -----------------------------------------------------------------------

/// A fake Anthropic key of the REAL length — 108 characters — because the
/// length is the whole point: it is what makes the value wrap at 120
/// columns, and a shorter stand-in would render on one row and prove
/// nothing.
///
/// The body is deterministic pseudo-random rather than a repeated
/// character so that no eight-character window of it occurs twice, which is
/// what lets [`assert_no_fragment_survives`] treat any surviving window as
/// evidence rather than as a coincidence. Never a real key.
const WRAPPING_FAKE_KEY: &str = "sk-ant-api03-FAKE-at88unjrP0OqBDXjbB87XI4kBCwkHjpvXQJBVsvFVWYuS6kfhXAxGYQJntl73YD1xRekgbsGPDiVfdPg7RMSHYKnu8";

/// The width the harness renders at, and the width the arithmetic in
/// `credential_redaction_ranges`' comment is stated for.
const GRID_COLS: u16 = 120;

/// Paint `rows` into a REAL vt100 screen the way ratatui paints — one
/// explicit cursor move per row, never relying on the terminal's own
/// auto-wrap — and return exactly what [`TuiDeck::snapshot_grid`] returns.
///
/// That distinction is load-bearing, not pedantry. `vt100` suppresses the
/// row separator for a row the TERMINAL wrapped itself, so a test that
/// wrote a long line and let the terminal wrap it would get the value back
/// CONTIGUOUS from `contents()`, pass against the byte-exact matcher, and
/// prove nothing at all. ratatui positions the cursor per row, so the deck's
/// grid always takes the separator path — measured both ways before this
/// test was written.
fn render_like_ratatui(cols: u16, rows: &[String]) -> String {
    let height = u16::try_from(rows.len().max(1)).expect("test grid height");
    let mut parser = vt100::Parser::new(height, cols, 0);
    for (index, row) in rows.iter().enumerate() {
        parser.process(format!("\x1b[{};1H{row}", index + 1).as_bytes());
    }
    parser.screen().contents()
}

/// Split `text` into rows of at most `width` CHARACTERS, the way a terminal
/// breaks a line that does not fit.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    text.chars()
        .collect::<Vec<char>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Every [`MIN_WRAP_FRAGMENT`]-character window of `secret` must be gone
/// from `text`.
///
/// Stronger than `!text.contains(secret)` on purpose: the failure this
/// guards is precisely that the value survives in PIECES, so asserting on
/// the whole value is the assertion that already passed vacuously while the
/// credential sat on screen in two rows. The window is the matcher's own
/// minimum fragment, so anything at least this long is something the
/// matcher was supposed to have caught.
fn assert_no_fragment_survives(text: &str, secret: &str, what: &str) {
    for window in secret.as_bytes().windows(MIN_WRAP_FRAGMENT) {
        let fragment = std::str::from_utf8(window).expect("the fixtures are ASCII");
        assert!(
            !text.contains(fragment),
            "{what}: a fragment of {MIN_WRAP_FRAGMENT} credential characters survived \
             redaction, so the wrapped value is still reconstructable from this text:\n{text}"
        );
    }
}

/// Scenario: Render `ANTHROPIC_API_KEY=<108-character key>` into a real
/// 120-column vt100 screen painted row by row, the way the deck paints, so
/// the key breaks 102 / 6 and its 20-character response id breaks 14 / 6.
/// Confirm the raw grid really is split that way — neither registered
/// pattern occurs in it — and then confirm that no eight-character piece of
/// the key survives redaction into `final-grid.txt`, into the SVG rendered
/// from it, or into a panic message.
///
/// This is issue #785 blocker A stated as a test. The whole credential was
/// reconstructable out of a panic grid while both registered patterns
/// matched nothing, and a panic grid goes two places: nextest's live console
/// output, which lands in the developer's own terminal scrollback, and the
/// raw JUnit report, which is a file on that same machine. Since #502 lane 2
/// runs on no runner, so neither passes through a masking layer, and the
/// masking that does exist where logs are rendered covers a registered
/// secret's exact value rather than reassembling line-wrapped fragments of
/// it.
#[test]
fn a_credential_wrapped_across_grid_rows_is_redacted_from_every_sink() {
    let key = WRAPPING_FAKE_KEY;
    assert_eq!(key.chars().count(), 108, "the fixture must be key-length");
    let id = claude_api_key_response_id(key);
    let redactions = api_key_recording_redactions(key);

    let rows = wrap_to_width(&format!("ANTHROPIC_API_KEY={key}"), GRID_COLS as usize);
    let grid = render_like_ratatui(GRID_COLS, &rows);

    // Non-vacuity, and the arithmetic itself: 120 columns less an
    // 18-character label leaves 102 of the key on the first row and 6 on
    // the second, which splits the response id 14 / 6.
    assert_eq!(rows.len(), 2, "the fixture must occupy exactly two rows");
    assert!(
        grid.contains(&key[..102]) && grid.contains(&key[102..]),
        "the render did not break the key 102 / 6, so this test is not \
         reproducing the reported shape:\n{grid}"
    );
    assert!(
        !grid.contains(key) && !grid.contains(&id),
        "the raw grid still carries a contiguous credential, so byte-exact \
         matching would already have caught it and this proves nothing:\n{grid}"
    );

    // The recording sink — `final-grid.txt` and the SVG rendered from it.
    let redacted = redact_known_credentials_text(&grid, &redactions);
    assert_no_fragment_survives(&redacted, key, "final-grid.txt");
    assert!(redacted.contains("[REDACTED-CREDENTIAL]"));
    let svg = render_grid_to_svg(&redacted, GRID_COLS, u16::try_from(rows.len()).unwrap());
    assert_no_fragment_survives(&svg, key, "final-grid.svg");

    // The diagnostic sink — the panic message the seam renders.
    register_diagnostic_redactions(redactions.clone());
    let panic_text = format_redacted_panic(
        "deck",
        "tests/common/mod.rs:1:1",
        &format!("did not see \"ready\" within 30s.\nFinal grid:\n{grid}"),
    );
    assert_no_fragment_survives(&panic_text, key, "the panic seam");
    assert!(
        panic_text.contains("did not see \"ready\" within 30s."),
        "the panic kept none of its diagnostic shape:\n{panic_text}"
    );
}

/// Scenario: The same 108-character key, but rendered the way the deck
/// actually renders — a sidebar column, a pane border, the value wrapped
/// inside the pane — so the bytes between two fragments of the credential
/// are somebody else's rendered content rather than a bare newline. It is
/// still redacted.
///
/// This is why the matcher bridges ARBITRARY bytes after a row transition
/// instead of a whitelist of frame characters. A whitelist would cover a
/// full-width dump and quietly miss the layout the deck spends most of its
/// time in, which is the one a panicking test screenshots.
#[test]
fn a_credential_wrapped_inside_a_bordered_pane_is_redacted_too() {
    let key = WRAPPING_FAKE_KEY;
    let redactions = api_key_recording_redactions(key);

    // 8-column sidebar + border + 110-column pane + border = 120.
    const INNER: usize = 110;
    let sidebar = ["worker-1", "worker-2", "orch    "];
    let pane = wrap_to_width(&format!("ANTHROPIC_API_KEY={key}"), INNER);
    assert_eq!(pane.len(), 2, "the value must wrap inside the pane");
    let rows: Vec<String> = pane
        .iter()
        .enumerate()
        .map(|(index, content)| format!("{:<8}\u{2502}{content:<INNER$}\u{2502}", sidebar[index]))
        .collect();
    let grid = render_like_ratatui(GRID_COLS, &rows);

    assert!(
        !grid.contains(key),
        "the raw grid still carries the key contiguously:\n{grid}"
    );
    assert!(
        grid.lines()
            .nth(1)
            .is_some_and(|row| row.starts_with("worker-2")),
        "the second row must OPEN with the sidebar, so the bytes between the \
         two fragments of the credential are somebody else's rendered content \
         rather than padding — otherwise this test is not about the gap it \
         claims to be about:\n{grid}"
    );

    let redacted = redact_known_credentials_text(&grid, &redactions);
    assert_no_fragment_survives(&redacted, key, "a bordered pane's grid");
    assert!(redacted.contains("[REDACTED-CREDENTIAL]"));
    // The layout survives: the redaction replaces the credential runs and
    // leaves the border, the sidebar and the row separator in place.
    assert!(
        redacted.contains("worker-1") && redacted.contains("worker-2"),
        "redaction ate the surrounding render:\n{redacted}"
    );
}

/// Scenario: A value split across two `.cast` events by the deck's own
/// cursor-position escape — the same wrap, one layer below the grid — is
/// redacted out of `full-stream.cast`.
///
/// `redact_cast_events` already concatenated the stream before matching, so
/// a value split across two PTY READS was covered. A value the deck itself
/// painted onto two rows was not: what sits between the fragments there is
/// an escape sequence, which is why [`is_row_transition`] counts `0x1b`.
#[test]
fn a_credential_painted_onto_two_cast_rows_is_redacted() {
    let key = WRAPPING_FAKE_KEY;
    let events = vec![
        CastEvent {
            offset_secs: 0.1,
            data: format!("\x1b[1;1HANTHROPIC_API_KEY={}", &key[..102]).into_bytes(),
        },
        CastEvent {
            offset_secs: 0.2,
            data: format!("\x1b[2;1H{} \x1b[K", &key[102..]).into_bytes(),
        },
    ];
    let joined: Vec<u8> = redact_cast_events(&events, &api_key_recording_redactions(key))
        .into_iter()
        .flatten()
        .collect();
    let text = String::from_utf8_lossy(&joined).into_owned();
    assert_no_fragment_survives(&text, key, "full-stream.cast");
    assert!(text.contains("[REDACTED-CREDENTIAL]"));
    assert!(
        text.contains("\x1b[2;1H"),
        "the escape that separates the rows must survive, or the cast stops \
         replaying:\n{text:?}"
    );
}

/// Scenario: Ask the matcher to bridge a gap it must NOT bridge. Two
/// unrelated four-character runs that happen to concatenate into a
/// registered value are left alone, because every hop has to reproduce at
/// least [`MIN_WRAP_FRAGMENT`] bytes.
///
/// The bound is what stops "match, skip, match again" from degenerating
/// into a subsequence search, and a subsequence search over a terminal grid
/// would redact arbitrary unrelated text.
#[test]
fn gap_bridging_does_not_degenerate_into_a_subsequence_search() {
    let credential = "abcdefghijklmnop".to_string();
    let registered = std::slice::from_ref(&credential);
    // Every fragment here is four bytes, half the minimum.
    let text = "abcd|xxxx\nefgh|xxxx\nijkl|xxxx\nmnop";
    assert_eq!(
        redact_known_credentials_text(text, registered),
        text,
        "four-byte fragments were chained into a match"
    );
    // The same value split at the minimum IS matched, so the bound is a
    // threshold rather than a refusal to bridge at all.
    let wrapped = "abcdefgh|pane\nijklmnop";
    let redacted = redact_known_credentials_text(wrapped, registered);
    assert!(
        !redacted.contains("abcdefgh") && !redacted.contains("ijklmnop"),
        "{redacted}"
    );
    assert!(
        redacted.contains("|pane\n"),
        "the bytes between the fragments must be preserved: {redacted}"
    );
}

/// Scenario: The 108-character key wrapped 102 / 6 the way the deck paints
/// it, with its OWN registered 20-character response id painted on the next
/// row AHEAD of the key's six-character continuation. Then the same
/// interleaving inside a bordered pane, where the width breaks the key
/// 92 / 16 so the continuation is long enough to be evidence on its own.
/// Neither registered value may survive either grid.
///
/// This is the hole the first version of this matcher left. The two values
/// are ALWAYS registered together (`api_key_recording_redactions`), and the
/// response id is a suffix of the key — so the run that resumes the key on
/// the next row also occurs inside the response id, a few characters
/// earlier. The matcher chained through that earlier occurrence, then
/// advanced past the whole match, and the bytes it had preserved in between
/// — which held the rest of the response id — were never scanned again. The
/// derivative stayed reconstructable while every contiguous assertion
/// passed.
///
/// The 92 / 16 half is the one that needs the alternate candidates: there
/// the run the chain consumed inside the response id is sixteen characters,
/// so the REAL continuation left behind is sixteen characters of key
/// material, well over [`MIN_WRAP_FRAGMENT`].
#[test]
fn two_registered_values_interleaved_across_rows_are_both_redacted() {
    let key = WRAPPING_FAKE_KEY;
    let redactions = api_key_recording_redactions(key);
    let id = claude_api_key_response_id(key);

    // The response id is a suffix of the key, which is exactly why the
    // key's continuation also occurs inside it. Pinned, because if that
    // ever stopped holding this test would still pass and prove nothing.
    assert!(key.ends_with(&id) && id.chars().count() == 20);

    for (label, label_width, continuation_at) in [
        ("a full-width grid", GRID_COLS as usize, 102usize),
        ("a bordered pane", 110, 92),
    ] {
        let head = &key[..continuation_at];
        let tail = &key[continuation_at..];
        let rows = wrap_to_width(&format!("ANTHROPIC_API_KEY={key}"), label_width);
        assert_eq!(rows.len(), 2, "{label}: the fixture must occupy two rows");
        assert_eq!(rows[1], tail, "{label}: the break is not where claimed");

        // Row two carries the approval prompt's response id first and the
        // wrapped value's continuation second, separated by a pane border —
        // the interleaving the matcher has to survive.
        let grid = render_like_ratatui(
            GRID_COLS,
            &[
                rows[0].clone(),
                format!("use this key? ...{id} \u{2502}{tail}"),
            ],
        );

        // Non-vacuity: the key really is broken, and the response id really
        // is present whole, so it is a value the matcher is obliged to find
        // rather than one this test smuggled past it.
        assert!(
            !grid.contains(key) && grid.contains(head) && grid.contains(&id),
            "{label}: the fixture does not carry the reported shape:\n{grid}"
        );

        let redacted = redact_known_credentials_text(&grid, &redactions);
        assert_no_fragment_survives(&redacted, key, label);
        assert_no_fragment_survives(&redacted, &id, label);
        // And the layout between the fragments is still there — the point
        // of fragment-aware matching is that a panic stays readable.
        assert!(
            redacted.contains("use this key? ") && redacted.contains('\u{2502}'),
            "{label}: redaction ate the surrounding render:\n{redacted}"
        );
    }
}

// -----------------------------------------------------------------------
// Issue #810 — the two reproduced leaks in the greedy scan
// -----------------------------------------------------------------------
//
// Both fixtures use FABRICATED values chosen so the geometry is readable in
// the assertion messages. No real credential shape is needed to provoke
// either leak: what provokes them is where the registered values overlap and
// where the continuations sit, not what the bytes mean.

/// A registered value that BEGINS inside another registered value's match.
/// Fabricated; the shared `ABCDEFGH` is the overlap the scan used to step
/// over.
const INTERIOR_START_FIRST: &str = "first-value-ABCDEFGH";
/// The second half of that pair — it starts at the shared run, so its own
/// start offset lies inside the first value's match.
const INTERIOR_START_SECOND: &str = "ABCDEFGH-second-value-tail";

/// Scenario: Register two overlapping fabricated values and scan their
/// overlap-sharing concatenation `first-value-ABCDEFGH-second-value-tail`.
/// The second value begins inside the first one's match. Both must be gone
/// from the redacted text.
///
/// This is issue #810's leak 1. The scan resumed at the smallest
/// `first_fragment_end` among the matches at a position, which reads the
/// bytes a fragmented match PRESERVED but still skips every offset INSIDE
/// its first fragment — so the second value's start, which sits in the
/// shared `ABCDEFGH`, was never a position the scan looked at. The artifact
/// came out `[REDACTED-CREDENTIAL]-second-value-tail`.
#[test]
fn a_registered_value_starting_inside_another_match_is_redacted_too() {
    let first = INTERIOR_START_FIRST.to_string();
    let second = INTERIOR_START_SECOND.to_string();
    let overlap = "ABCDEFGH";
    let text = format!(
        "{}{overlap}{}",
        first.strip_suffix(overlap).expect("the pair must overlap"),
        second.strip_prefix(overlap).expect("the pair must overlap"),
    );

    // Non-vacuity: both values really are present, contiguously, and they
    // really do share the run — so this is a fixture the matcher is obliged
    // to find rather than one built out of coincidences.
    assert!(
        text.contains(&first) && text.contains(&second),
        "the fixture does not carry both values contiguously: {text}"
    );
    assert!(
        text.find(&second).expect("present") < text.find(&first).expect("present") + first.len(),
        "the second value must START INSIDE the first one's match, or this \
         test is not about the reported shape: {text}"
    );

    let redacted = redact_known_credentials_text(&text, &[first.clone(), second.clone()]);
    assert_no_fragment_survives(&redacted, &first, "an interior-start pair");
    assert_no_fragment_survives(&redacted, &second, "an interior-start pair");
    assert!(
        !redacted.contains("second-value-tail"),
        "the second value's tail survived the scan: {redacted}"
    );
}

/// Scenario: Register a fabricated value whose continuation occurs TWICE
/// after the row transition — once as a decoy that reproduces the minimum
/// fragment and then dead-ends, and once as the genuine remainder. The value
/// must be redacted.
///
/// This is issue #810's leak 2. `credential_fragments_at` recorded every
/// candidate but followed `candidates.first()` and returned `None` when that
/// path dead-ended, so the decoy suppressed the real continuation and the
/// matcher redacted NOTHING AT ALL — the whole text came back unchanged.
/// Collecting candidates is not the same as searching exhaustively while one
/// candidate still decides whether any result exists.
#[test]
fn a_decoy_continuation_does_not_suppress_the_real_one() {
    let credential = "abcdefghABCDEFGHijklmnop".to_string();
    let registered = std::slice::from_ref(&credential);
    // `abcdefgh` wraps to the next row. There the continuation `ABCDEFGH…`
    // occurs first as a decoy that cannot reach `ijklmnop`, and only after
    // it as the genuine sixteen-byte remainder.
    let text = "abcdefgh\nABCDEFGH decoy ABCDEFGHijklmnop";

    // Non-vacuity: the value is nowhere contiguous, so byte-exact matching
    // would find nothing and the fragment matcher is the thing under test.
    assert!(
        !text.contains(&credential),
        "the fixture carries the value contiguously and proves nothing: {text}"
    );
    assert_eq!(
        text.matches("ABCDEFGH").count(),
        2,
        "the fixture must offer a decoy AND a real continuation: {text}"
    );

    let redacted = redact_known_credentials_text(text, registered);
    assert_ne!(
        redacted, text,
        "the decoy suppressed the real continuation and nothing was redacted"
    );
    assert_no_fragment_survives(&redacted, &credential, "a decoy continuation");
    assert!(
        redacted.contains(" decoy "),
        "the bytes between the fragments must be preserved: {redacted}"
    );
}

/// Scenario: Redact the interior-start fixture twice — once with only the
/// value whose start is interior, then with both values — and assert the
/// second run redacts a SUPERSET of the bytes the first run redacted.
///
/// This is the monotonicity property issue #810 requires of the rewrite, and
/// it is a property to assert rather than a mechanism to design: an
/// all-match search that emits ranges from every complete path has it for
/// free. It has to be asserted because the greedy scan did NOT have it, and
/// [`TuiDeck::artifact_redactions`] unions the per-deck set with the
/// process-global diagnostic store — so under leak 1 enlarging the set could
/// REMOVE a redaction, which is the opposite of what a union is for.
#[test]
fn enlarging_the_registered_set_never_removes_a_redaction() {
    let first = INTERIOR_START_FIRST.to_string();
    let second = INTERIOR_START_SECOND.to_string();
    let text = "first-value-ABCDEFGH-second-value-tail";

    let smaller = credential_redaction_ranges(text.as_bytes(), std::slice::from_ref(&second));
    let larger = credential_redaction_ranges(text.as_bytes(), &[first, second]);

    // Non-vacuity: the smaller set really does redact something, so the
    // superset assertion has something to be a superset of.
    assert!(
        !smaller.is_empty(),
        "the smaller set redacted nothing, so this proves nothing"
    );

    let covered = |ranges: &[(usize, usize)], byte: usize| {
        ranges
            .iter()
            .any(|(start, end)| (*start..*end).contains(&byte))
    };
    for byte in 0..text.len() {
        assert!(
            !covered(&smaller, byte) || covered(&larger, byte),
            "byte {byte} of {text:?} is redacted with one registered value and \
             NOT with two, so adding a pattern removed a redaction: \
             {smaller:?} then {larger:?}"
        );
    }
}

/// The `~/.claude/.credentials.json` document as a terminal would paint it —
/// wrapped at the harness's own width, so each token is broken across rows.
fn rendered_credential_document() -> String {
    wrap_to_width(FABRICATED_CLAUDE_CREDENTIALS, GRID_COLS as usize).join("\n")
}

/// The pair the cost of this matcher has always been measured with: two
/// fabricated 128-character tokens that share their first eight bytes
/// (`sk-ant-o`), so every occurrence of that prefix is a start for BOTH.
fn fabricated_token_pair() -> Vec<String> {
    vec![
        FABRICATED_CLAUDE_ACCESS_TOKEN.to_string(),
        FABRICATED_CLAUDE_REFRESH_TOKEN.to_string(),
    ]
}

/// Grow `artifact` to at least `bytes` by repeating `row`.
fn pad_to(artifact: &mut String, row: &str, bytes: usize) {
    while artifact.len() < bytes {
        artifact.push_str(row);
    }
}

/// Scenario: Scan the three artifact shapes issue #810's cost data names — a
/// realistic stream holding one rendered credential document, the same document
/// packed every 140 bytes, and the shared eight-byte token prefix repeated so
/// it never chains into anything — and confirm each completes well inside its
/// [`match_step_budget`] rather than refusing, finding the document where there
/// is one.
///
/// The third shape is the reason the budget and the index both exist. Under the
/// greedy scan 1.6 MB of it took **25.3 seconds**, because every occurrence of a
/// shared prefix paid a full `MAX_WRAP_GAP` candidate sweep per pattern whether
/// or not a range came of it — an availability problem on an artifact dump,
/// since `.cast` history is not size-bounded and an agent that can render the
/// prefix can produce far more than 1.6 MB.
///
/// It asserts on STEPS rather than on a clock, because a step count is
/// deterministic and a wall time is not: a hop that went back to re-reading
/// `MAX_WRAP_GAP` bytes would show up here on a machine of any speed under any
/// load. The wall times measured at the full sizes are on
/// [`credential_redaction_scan`]; the fixtures here are a tenth of those, which
/// is where the ratio stops changing and keeps the test off the fast tier's
/// critical path.
#[test]
fn the_measured_artifact_shapes_scan_inside_the_budget() {
    let registered = fabricated_token_pair();
    let document = rendered_credential_document();

    // Realistic: one rendered document inside ordinary render.
    let mut realistic = String::new();
    pad_to(
        &mut realistic,
        "worker-1 \u{2502} waiting for the agent to report ready \u{2502}\n",
        100_000,
    );
    realistic.push_str(&document);
    pad_to(
        &mut realistic,
        "worker-2 \u{2502} waiting for the agent to report ready \u{2502}\n",
        200_000,
    );

    // Packed: the same document every 140 bytes — the densest shape measured.
    let mut packed = String::new();
    pad_to(
        &mut packed,
        &format!("{document}\n-- 140 bytes of pane chrome between two renders ------------\n"),
        200_000,
    );

    // Adversarial: the shared prefix over and over, chaining into nothing.
    let mut adversarial = String::new();
    pad_to(
        &mut adversarial,
        "sk-ant-o\u{2502}nothing follows this\n",
        200_000,
    );

    for (what, artifact, must_find) in [
        ("a realistic artifact", realistic, true),
        ("a packed artifact", packed, true),
        ("the adversarial shape", adversarial, false),
    ] {
        // Non-vacuity: the shapes really are big enough to be the shapes they
        // claim, so a budget expressed per byte has something to bound.
        assert!(
            artifact.len() > 150_000,
            "{what}: the fixture is too small to be the shape it claims"
        );
        let ranges = match credential_redaction_scan(artifact.as_bytes(), &registered) {
            RedactionScan::Ranges(ranges) => ranges,
            RedactionScan::Refused => panic!(
                "{what}: the scan exhausted its budget on a shape it is sized \
                 for, so every artifact of this shape would be withheld whole"
            ),
        };
        assert_eq!(
            !ranges.is_empty(),
            must_find,
            "{what}: the scan found {} ranges",
            ranges.len()
        );

        let budget = match_step_budget(artifact.len());
        let cost = credential_scan_spend(artifact.as_bytes(), &registered);
        assert!(
            cost * 2 < budget,
            "{what}: {cost} steps over {} bytes is more than half the {budget} \
             the budget allows — this shape used to sit far below it, so \
             something reintroduced per-hop rescanning",
            artifact.len()
        );
    }
}

/// Scenario: Redact over artifacts SHORTER than one [`MIN_WRAP_FRAGMENT`]
/// window, from empty up to seven bytes, with values registered that could
/// match part of them. Nothing panics and nothing is mangled.
///
/// The index walks a rolling four-byte window and probes an eight-byte one, so
/// every artifact between four and seven bytes long has a window that opens but
/// no needle that fits. Cheap to get wrong and impossible to notice: a
/// `fixture.toml` is never that short, but a panic payload can be, and the seam
/// that redacts panics is the one that must not panic.
#[test]
fn an_artifact_shorter_than_one_window_is_redacted_without_panicking() {
    let registered = vec!["abcdefghijklmnop".to_string(), "abc".to_string()];
    for length in 0..=7usize {
        let artifact = "abcdefg"[..length].to_string();
        let redacted = redact_known_credentials_text(&artifact, &registered);
        // Only the short registered value can occur, and only contiguously.
        let expected = match length {
            0..=2 => artifact.clone(),
            _ => artifact.replacen("abc", "[REDACTED-CREDENTIAL]", 1),
        };
        assert_eq!(
            redacted, expected,
            "a {length}-byte artifact came back wrong"
        );
    }
}

/// Scenario: Register a fabricated value far longer than any credential — the
/// shape a mis-registration produces, a whole configuration blob read as one
/// JSON string — and confirm a contiguous occurrence is still redacted while a
/// wrapped one is left alone, with nothing overflowing on the way.
///
/// This pins `MAX_CHAINED_VALUE`, which exists because the continuation search
/// recurses once per hop: without it a registered value of a few hundred
/// kibibytes would exhaust the stack inside the panic seam, and the panic seam
/// is the one place in this harness that must not itself crash. The wrapped half
/// asserts a LIMIT rather than a guarantee, and is here so that limit is written
/// down where someone changing the constant will see it.
#[test]
fn a_value_far_longer_than_any_credential_keeps_byte_exact_redaction() {
    // The unique `HEADER01` opening matters: a value whose own opening window
    // recurs inside it is a start at every recurrence, and a 9 KB value repeated
    // that way is an adversarial fixture in its own right — it exhausts the step
    // budget and this test then measures the budget instead of the bound.
    let value = format!("HEADER01{}", "abcdefgh-ijklmnop-".repeat(600));
    assert!(
        value.len() > 8192 && value.matches("HEADER01").count() == 1,
        "the fixture must exceed MAX_CHAINED_VALUE, and open uniquely"
    );
    let registered = std::slice::from_ref(&value);

    let contiguous = format!("before {value} after");
    assert_eq!(
        redact_known_credentials_text(&contiguous, registered),
        "before [REDACTED-CREDENTIAL] after",
        "a contiguous occurrence of an over-long value stopped being redacted"
    );

    // The documented limit: chained across a row transition, it is not matched.
    let split = value.len() / 2;
    let wrapped = format!("{}\n{}", &value[..split], &value[split..]);
    assert_eq!(
        redact_known_credentials_text(&wrapped, registered),
        wrapped,
        "an over-long value was chained across rows, so MAX_CHAINED_VALUE is \
         no longer bounding the recursion this test exists to bound"
    );
}

/// Scenario: Hand the scan an artifact built to make its continuation search
/// blow up — a fabricated value whose two halves are each rendered on their own
/// row, over and over, so every start sees hundreds of places to resume — and
/// confirm it REFUSES, and that the refusal replaces the whole artifact instead
/// of writing the part it managed to scan.
///
/// This is the fail-closed half of issue #810, and it is a test rather than a
/// comment because the failure it guards is silent: a budget that returned the
/// ranges it had found so far would write every byte it never looked at, which
/// is the leak the budget exists to prevent rather than a degraded version of
/// preventing it.
#[test]
fn an_artifact_that_exhausts_the_budget_is_withheld_whole() {
    // Fabricated, and shaped for the blow-up rather than for realism: each half
    // is exactly `MIN_WRAP_FRAGMENT`, and each row offers another place the
    // second half could resume, so the candidates at one state grow with
    // `MAX_WRAP_GAP` rather than staying at one or two.
    let value = "AAAAAAAABBBBBBBB".to_string();
    let registered = std::slice::from_ref(&value);
    let mut artifact = String::new();
    pad_to(&mut artifact, "AAAAAAAA\nBBBBBBBB\n", 40_000);

    // Non-vacuity: a small artifact of the SAME shape is scanned rather than
    // refused, so what follows is a budget and not a blanket refusal.
    let mut small = String::new();
    pad_to(&mut small, "AAAAAAAA\nBBBBBBBB\n", 1_000);
    assert!(
        matches!(
            credential_redaction_scan(small.as_bytes(), registered),
            RedactionScan::Ranges(_)
        ),
        "the scan refuses even a small artifact of this shape, so the refusal \
         below proves nothing about the budget"
    );

    assert!(
        matches!(
            credential_redaction_scan(artifact.as_bytes(), registered),
            RedactionScan::Refused
        ),
        "the scan did not refuse an artifact whose search exceeds its budget"
    );

    let written = redact_known_credentials_bytes(artifact.as_bytes(), registered);
    assert_eq!(
        written, RECORDING_CREDENTIAL_SCAN_REFUSAL,
        "a refused scan wrote something other than the whole-artifact refusal"
    );
    assert!(
        !String::from_utf8_lossy(&written).contains("AAAAAAAA"),
        "a refused scan wrote unscanned bytes from the artifact"
    );

    // The same refusal reaches `full-stream.cast`, which redacts across the
    // concatenated stream and therefore cannot vouch for any single event.
    let events = vec![
        CastEvent {
            offset_secs: 0.1,
            data: artifact.as_bytes().to_vec(),
        },
        CastEvent {
            offset_secs: 0.2,
            data: b"a later event".to_vec(),
        },
    ];
    let projected = redact_cast_events(&events, registered);
    assert_eq!(projected.len(), events.len(), "an event was dropped");
    assert_eq!(projected[0], RECORDING_CREDENTIAL_SCAN_REFUSAL);
    assert!(
        projected[1].is_empty(),
        "a refused cast kept an event the scan never vouched for"
    );
}

/// Scenario: Build the shape where one candidate enumeration is expensive — a
/// value whose continuation is four thousand identical bytes, rendered so that
/// nearly every offset in the gap is another place it could resume — and confirm
/// the refused scan stops INSIDE that enumeration rather than after it.
///
/// The bound asserted is "budget plus at most one value length": a run is
/// measured before it is charged, so exactly one `common_prefix_len` can finish
/// past the point of refusal, and nothing else. Charging only on the way out of
/// `hop_candidates` — which is what this code did before Greptile's P2 on PR
/// #894 — lets a single call spend millions of steps on a forty-kilobyte
/// artifact whose whole allowance is a couple of hundred thousand, which is a
/// fail-closed bound that fails slowly.
#[test]
fn a_refused_scan_stops_inside_the_candidate_enumeration() {
    // Fabricated. `AAAAAAAA` opens it; the rest is one long run, so every one
    // of the ~4000 offsets inside a rendered copy is a candidate resume point
    // AND each one reproduces thousands of bytes.
    let value = format!("AAAAAAAA{}", "B".repeat(4000));
    let registered = std::slice::from_ref(&value);
    let mut artifact = String::new();
    pad_to(
        &mut artifact,
        &format!("AAAAAAAA\n{}\n", "B".repeat(4000)),
        40_000,
    );

    assert!(
        matches!(
            credential_redaction_scan(artifact.as_bytes(), registered),
            RedactionScan::Refused
        ),
        "the fixture must be one the scan refuses, or the bound below is \
         asserted about a scan that never hit its budget"
    );
    let budget = match_step_budget(artifact.len());
    let spent = credential_scan_spend(artifact.as_bytes(), registered);
    assert!(
        spent <= budget + value.len(),
        "a refused scan spent {spent} against a budget of {budget}, overshooting \
         by more than the one run it is allowed to finish measuring — the budget \
         is no longer being charged inside the candidate enumeration"
    );
}

/// Scenario: Register far more credential material than the index will hold —
/// three hundred distinct thousand-character values — against a TINY artifact,
/// and confirm the scan refuses rather than building the index anyway.
///
/// The artifact side of the index has always been capped; the pattern side was
/// not, and `collect_credential_values` accepts an unbounded number of values
/// with no upper length filter. The size that matters is therefore the
/// REGISTERED SET, not the artifact — which is why the fixture here is a
/// two-hundred-byte string, the size of a panic message. Greptile's P2 on PR
/// #894.
#[test]
fn a_registered_set_too_large_to_index_is_refused_before_it_is_built() {
    let artifact = "a panic message is a few hundred bytes, and the seam that \
                    redacts one is supposed to be cheap";
    let many: Vec<String> = (0..300)
        .map(|index| format!("{index:04}{}", "credential-material-".repeat(49)))
        .collect();
    // Non-vacuity: each value is under `MAX_CHAINED_VALUE`, so this is the
    // pattern-COUNT bound rather than the per-value one, and the artifact is
    // far too small to be what exhausts anything.
    assert!(
        many.iter()
            .all(|value| value.len() > 900 && value.len() < 8192),
        "the fixture must be many ordinary-length values"
    );
    assert!(artifact.len() < 200, "the artifact must be tiny");

    assert!(
        matches!(
            credential_redaction_scan(artifact.as_bytes(), &many),
            RedactionScan::Refused
        ),
        "an unindexable registered set was scanned instead of refused"
    );
    assert_eq!(
        redact_known_credentials_text(artifact, &many),
        String::from_utf8_lossy(RECORDING_CREDENTIAL_SCAN_REFUSAL),
        "a refused scan wrote something other than the whole-artifact refusal"
    );

    // A tenth of that set is indexed and scanned normally, so the refusal above
    // is a cap and not a blanket rejection of large sets.
    assert!(
        matches!(
            credential_redaction_scan(artifact.as_bytes(), &many[..30]),
            RedactionScan::Ranges(_)
        ),
        "the scan refuses a set it should comfortably index"
    );
}

// -----------------------------------------------------------------------
// Issue #502 — the credential reaches the agent that needs it
// -----------------------------------------------------------------------

/// Scenario: The Anthropic API key is on the list of variables that cross
/// the harness's `env_clear`, so a deck — and the daemon it `setsid`s away
/// and the agents that daemon spawns — inherits it.
///
/// Pinned rather than reviewed because the failure is silent and in the
/// direction that erases coverage: without it, `check_claude_available`'s
/// API-key path passes a gate the spawned agent then cannot satisfy, so a
/// key-authenticated lane-2 run stalls in a PTY wait instead of saying what
/// is missing.
#[test]
fn the_credential_crosses_the_harness_env_clear() {
    assert!(
        INHERIT_PASS.contains(&ANTHROPIC_API_KEY_ENV),
        "the credential a key-authorised lane-2 run depends on no longer \
         reaches a deck-spawned agent: {INHERIT_PASS:?}"
    );
}

// -----------------------------------------------------------------------
// Issue #502/#785 audit S1 — the DIAGNOSTIC redaction seam
// -----------------------------------------------------------------------

/// Marker the child half below puts in its panic message so the parent can
/// tell "the child panicked where it was supposed to" apart from "the child
/// died on the way there".
#[cfg(unix)]
const PANIC_SEAM_MARKER: &str = "harness-panic-seam-reached";
/// Set by the parent on the child it spawns. Without it the child half is a
/// no-op, so the test is inert under `--run-ignored all`, under a plain
/// `cargo test`, and anywhere else it is selected on its own.
#[cfg(unix)]
const PANIC_SEAM_CHILD_ENV: &str = "DAD_TEST_PANIC_SEAM_CHILD";
/// Deliberately fake, and deliberately the REAL length of an Anthropic key
/// so the child below can render both shapes that reach a panic message: the
/// approval prompt's contiguous 20-character response id, and the same key
/// broken across two 120-column rows. Never a real key: proving the seam
/// must not require writing a live credential into any captured output,
/// even a passing one.
#[cfg(unix)]
const PANIC_SEAM_FAKE_KEY: &str = WRAPPING_FAKE_KEY;

/// Scenario: Formatting a panic message that carries a registered
/// credential redacts it, while leaving the rest of the message — the
/// thread, the location, the surrounding diagnostic text — intact, because
/// a redacted panic still has to be readable enough to debug from.
#[test]
fn a_panic_message_is_redacted_without_losing_its_diagnostic_shape() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let suffix = claude_api_key_response_id(key);
    register_diagnostic_redactions(api_key_recording_redactions(key));

    let rendered = format_redacted_panic(
        "deck",
        "tests/common/mod.rs:1:1",
        &format!(
            "did not see \"ready\" within 30s.\nFinal grid:\n  \
             ANTHROPIC_API_KEY: sk-ant-...{suffix}\n  Do you want to use this API key?\n"
        ),
    );
    assert!(
        !rendered.contains(&suffix) && !rendered.contains(key),
        "a registered credential survived the panic seam: {rendered}"
    );
    assert!(rendered.contains("[REDACTED-CREDENTIAL]"), "{rendered}");
    assert!(rendered.contains("thread 'deck' panicked at tests/common/mod.rs:1:1"));
    assert!(rendered.contains("did not see \"ready\" within 30s."));
    assert!(rendered.contains("Do you want to use this API key?"));
}

/// Scenario: The child half of
/// [`the_panic_seam_redacts_a_key_this_process_read_from_the_environment`].
/// Runs only when the parent marks it, seeds a Claude worker HOME through
/// the REAL entry point (`seed_claude_worker_home` — the in-process-daemon
/// route that has no `TuiDeck` and so no recording redactions at all),
/// renders the approval prompt's key suffix into a grid, and dies at a PTY
/// wait the way a real one does.
///
/// Nothing here registers a redaction by hand. That is the point: what is
/// under test is the WIRING — that a real harness entry point reads the key
/// out of the environment and installs the seam before any diagnostic can
/// carry it.
///
/// UNIX-ONLY, and the reason is the entry point rather than the seam.
/// `seed_claude_worker_home` imports the host's Claude credentials, so it
/// resolves `host_home()` from `HOME` — which Windows does not set, making
/// this a hard panic there before the test reaches what it is testing.
/// Measured on `build-windows` for PR #805: 11 binaries failed on `HOME is
/// set on the host`. Gating rather than teaching `host_home()` about
/// `USERPROFILE`, because the L2 tier is Unix-only in practice (CLAUDE.md
/// rule 2, which is where that is recorded — rule 5 does not say it) and
/// inventing a Windows credential path to satisfy a
/// test would be a fiction. The seam ITSELF is platform-independent and
/// stays covered everywhere by
/// `a_panic_message_is_redacted_without_losing_its_diagnostic_shape` and by
/// the four wrapped-credential tests above, none of which touch `HOME`.
#[cfg(unix)]
#[test]
fn a_rendered_credential_reaches_a_panic_message_only_redacted() {
    if std::env::var_os(PANIC_SEAM_CHILD_ENV).is_none() {
        return;
    }
    let home = race_safe_tempdir();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).expect("child work dir");
    seed_claude_worker_home(home.path(), &[work.to_string_lossy().into_owned()])
        .expect("seed the isolated worker HOME");

    let key = std::env::var(ANTHROPIC_API_KEY_ENV).expect("the parent set a key");
    // A REAL 120-column vt100 render, painted row by row the way ratatui
    // paints, carrying both shapes: the approval prompt's contiguous
    // response id, and an `ANTHROPIC_API_KEY=` line that the width breaks
    // 102 / 6 (issue #785 blocker A). Neither may reach captured output.
    let mut rows = vec![
        "Detected a custom API key in your environment".to_string(),
        format!(
            "  ANTHROPIC_API_KEY: sk-ant-...{}",
            claude_api_key_response_id(&key)
        ),
        "  Do you want to use this API key?".to_string(),
        "   Yes".to_string(),
        " > No (recommended)".to_string(),
    ];
    rows.extend(wrap_to_width(
        &format!("ANTHROPIC_API_KEY={key}"),
        GRID_COLS as usize,
    ));
    let grid = render_like_ratatui(GRID_COLS, &rows);
    panic!("did not see \"ready\" within 30s. {PANIC_SEAM_MARKER}\nFinal grid:\n{grid}");
}

/// Scenario: Re-run this binary against the child above with a FAKE
/// `ANTHROPIC_API_KEY` in its environment, and read its captured output.
/// The rendered key suffix — the exact derivative Claude Code's approval
/// prompt paints, and the one GitHub's secret masking does not cover — must
/// not appear anywhere in what nextest would capture.
///
/// A fresh process is what makes this a test of the wiring rather than of
/// the redactor: the environment read, the hook installation and the panic
/// all happen in a process that started with nothing registered.
///
/// Gated with its child for one further reason beyond the child's own: with
/// only the child gated, the `--exact` filter here would match no test on
/// Windows and this half would fail on its own non-vacuity assertion,
/// reporting "the child never reached its panic" for a child that was never
/// compiled.
#[cfg(unix)]
#[test]
fn the_panic_seam_redacts_a_key_this_process_read_from_the_environment() {
    // This test's OWN assertion messages interpolate the child's captured
    // output, so install the seam here too. The child cannot carry the real
    // ambient key (the fake one is forced into its environment below), so
    // this is belt-and-braces rather than the load-bearing part — but a
    // diagnostic that renders another process's output is exactly the shape
    // this seam exists for, and it should not be the one place that opts
    // out.
    install_credential_redaction();
    let exe = std::env::current_exe().expect("current exe");
    // libtest test names omit the crate segment `module_path!()` carries.
    let module = module_path!()
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| module_path!());
    let child_test =
        format!("{module}::a_rendered_credential_reaches_a_panic_message_only_redacted");
    let out = std::process::Command::new(&exe)
        .arg(&child_test)
        .args(["--exact", "--test-threads=1", "--nocapture"])
        .env(PANIC_SEAM_CHILD_ENV, "1")
        .env(ANTHROPIC_API_KEY_ENV, PANIC_SEAM_FAKE_KEY)
        .env("RUST_BACKTRACE", "0")
        .output()
        .expect("re-run this test binary");
    let captured = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        captured.contains(PANIC_SEAM_MARKER),
        "the child never reached its panic, so this proves nothing — did \
         `{child_test}` match no tests, or did the HOME seeding fail?\n{captured}"
    );
    assert!(
        !out.status.success(),
        "the child was supposed to die on its panic: {}\n{captured}",
        out.status
    );
    let suffix = claude_api_key_response_id(PANIC_SEAM_FAKE_KEY);
    assert!(
        !captured.contains(suffix.as_str()),
        "the approval prompt's key suffix reached captured test output \
         unredacted — this is the sink the whole seam exists for\n{captured}"
    );
    assert!(
        !captured.contains(PANIC_SEAM_FAKE_KEY),
        "the key itself reached captured test output\n{captured}"
    );
    // Issue #785 blocker A: the child's grid also carries the key BROKEN
    // 102 / 6 across two 120-column rows, and neither of the two assertions
    // above can see that — they look for contiguous values. This one is the
    // one that fails if the wrapped half regresses. nextest writes exactly
    // these bytes into the raw JUnit report's captured-output element, so
    // this covers that sink and the run log with one assertion.
    assert_no_fragment_survives(
        &captured,
        PANIC_SEAM_FAKE_KEY,
        "the child's captured output",
    );
    // Non-vacuity, and specifically that the WRAPPED half was exercised:
    // one marker for the prompt's contiguous response id, and one per row
    // of the broken key.
    let markers = captured.matches("[REDACTED-CREDENTIAL]").count();
    assert!(
        markers >= 3,
        "expected at least three redactions — the prompt's response id plus \
         one per row of the wrapped key — but found {markers}, so the grid \
         never carried what this test is about and the assertions above \
         passed vacuously\n{captured}"
    );
}

/// The accessors that hand a caller raw terminal content. `registry.snapshot`
/// is the in-process-daemon route (`e2e_pi_orchestrator.rs`,
/// `e2e_delegate_work_done_chain.rs`, `e2e_codex_worker.rs`); the other two
/// are `TuiDeck`'s.
const TERMINAL_CONTENT_ACCESSORS: [&str; 3] = ["snapshot_grid", "stream_text", "registry.snapshot"];

/// Whether `haystack` mentions `ident` as a whole identifier — not inside
/// `grid_lines`, and not as the method in `sock.as_os_str().len()`, which is
/// why a leading `.` disqualifies a hit as firmly as a letter does.
fn mentions_ident(haystack: &str, ident: &str) -> bool {
    let mut from = 0;
    while let Some(found) = haystack[from..].find(ident) {
        let start = from + found;
        from = start + ident.len();
        let before = |c: char| !c.is_alphanumeric() && c != '_' && c != '.';
        let after = |c: char| !c.is_alphanumeric() && c != '_';
        if haystack[..start].chars().next_back().is_none_or(before)
            && haystack[from..].chars().next().is_none_or(after)
        {
            return true;
        }
    }
    false
}

/// The parts of a macro argument that carry CODE rather than prose: every
/// `{…}` placeholder, plus everything outside the string literals.
///
/// Matching a binding name against the whole argument reads the format
/// string's English too, and `"… seen live on grid = {saw_sentinel_grid}"`
/// in `e2e_pi_live.rs` is then reported for the word "grid" in a sentence.
/// Splitting the argument first is what keeps the binding arm precise enough
/// to be worth having. `{{` is skipped because it is an escaped brace, and a
/// `\'"\'` char literal inside a print argument would confuse the string
/// tracking — neither occurs in this suite.
fn interpolated_regions(arg: &str) -> Vec<&str> {
    let bytes = arg.as_bytes();
    let mut regions = Vec::new();
    let (mut index, mut in_string, mut escaped, mut outside_start) = (0usize, false, false, 0);
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                outside_start = index + 1;
            } else if byte == b'{' {
                if bytes.get(index + 1) == Some(&b'{') {
                    index += 2;
                    continue;
                }
                if let Some(close) = arg[index + 1..].find('}') {
                    regions.push(&arg[index + 1..index + 1 + close]);
                    index += 1 + close;
                }
            }
        } else if byte == b'"' {
            regions.push(&arg[outside_start..index]);
            in_string = true;
        }
        index += 1;
    }
    regions.push(&arg[outside_start.min(arg.len())..]);
    regions
}

/// The local bindings in `src` whose initialiser reads terminal content —
/// `let grid = deck.snapshot_grid();` and its variants.
///
/// Without this the scan below sees only what is written INSIDE the macro
/// call, and `let grid = deck.snapshot_grid(); eprintln!("{grid}");` walks
/// straight past it. Binding first is not a contrived way to write it; it
/// is how every wait helper in this harness already holds a grid before
/// interpolating it, so the gap sat exactly where the real code lives.
///
/// Names are collected per FILE rather than per function, which can only
/// over-report — and the remedy for a false positive is the same wrap the
/// true positive needs, so over-reporting costs nothing but a rename.
fn terminal_content_bindings(src: &str) -> Vec<String> {
    let mut bound: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(found) = src[from..].find("let ") {
        let start = from + found;
        from = start + "let ".len();
        // A `let` that starts a word, not the tail of an identifier.
        if src[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let mut rest = src[from..].trim_start();
        if let Some(stripped) = rest.strip_prefix("mut ") {
            rest = stripped.trim_start();
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // Destructuring patterns and `let else` are deliberately skipped:
        // neither binds a single name this scan could then look for.
        if name.is_empty() || name == "else" {
            continue;
        }
        let Some(end) = rest[name.len()..].find(';') else {
            continue;
        };
        let initialiser = &rest[name.len()..name.len() + end];
        if initialiser.contains("redact_credentials_for_output") {
            continue;
        }
        // A LENGTH is not content. `let len = registry.snapshot(id).map(|s|
        // s.len()).unwrap_or(0)` mentions an accessor and holds a number,
        // and tainting `len` file-wide reports every unrelated `{}` that
        // interpolates one. The cost of the shortcut is a binding that both
        // reduces to a scalar and keeps the content — none exists here, and
        // the accessor arm still covers an accessor written in the macro.
        if initialiser.contains(".len()") || initialiser.contains(".count()") {
            continue;
        }
        if TERMINAL_CONTENT_ACCESSORS
            .iter()
            .any(|accessor| initialiser.contains(accessor))
        {
            bound.push(name);
        }
    }
    bound.sort();
    bound.dedup();
    bound
}

/// Scenario: Scan every test source for a `print!`/`println!`/`eprint!`/
/// `eprintln!` that renders terminal content — either by calling an
/// accessor inside the macro, or by interpolating a local the file bound
/// from one. The redacting panic hook covers panics and assertion failures
/// — every diagnostic this suite actually produces — but it cannot cover a
/// direct write to stdout, so this keeps the invariant it relies on true as
/// the suite grows.
///
/// The remedy is named in the failure message rather than left to be
/// guessed: wrap the argument in `common::redact_credentials_for_output`,
/// which is exactly what the seam does for a panic.
#[test]
fn no_test_prints_terminal_content_outside_the_redacting_panic_seam() {
    /// The balanced `(…)` of a macro invocation starting at `open`, skipping
    /// parentheses inside string and character literals.
    fn macro_arg(src: &str, open: usize) -> &str {
        let bytes = src.as_bytes();
        let (mut depth, mut in_str, mut escaped, mut i) = (0usize, false, false, open);
        while i < bytes.len() {
            let c = bytes[i];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    // `'('` / `'\''` — a char literal, not a lifetime.
                    b'\'' if bytes.get(i + 2) == Some(&b'\'') => i += 2,
                    b'\'' if bytes.get(i + 3) == Some(&b'\'') && bytes[i + 1] == b'\\' => i += 3,
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return &src[open..=i];
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        &src[open..]
    }

    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut sources: Vec<PathBuf> = std::fs::read_dir(&tests_dir)
        .expect("read tests/")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "rs").then_some(path)
        })
        .collect();
    sources.push(tests_dir.join("common").join("mod.rs"));
    sources.sort();
    assert!(
        sources.len() > 50,
        "only found {} test sources under {} — the scan is not reaching the suite",
        sources.len(),
        tests_dir.display()
    );

    let mut offenders = Vec::new();
    for path in &sources {
        let src = std::fs::read_to_string(path).expect("read a test source");
        let bindings = terminal_content_bindings(&src);
        for name in ["println!", "eprintln!", "print!", "eprint!"] {
            let mut from = 0;
            while let Some(found) = src[from..].find(name) {
                let start = from + found;
                from = start + name.len();
                // `eprintln!` CONTAINS `println!`, so the needle has to
                // start at a macro boundary rather than mid-identifier, or
                // one `eprintln!` is reported twice under two names.
                if src[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    continue;
                }
                // A macro named inside a LINE COMMENT executes nothing, and
                // the doc comment on `terminal_content_bindings` above spells
                // out the exact pattern this scan looks for — so without this
                // the scanner reports its own documentation. Line comments
                // only: a block comment would need real lexing, and none in
                // this suite contains a print macro.
                let line_start = src[..start].rfind('\n').map_or(0, |at| at + 1);
                if src[line_start..start].contains("//") {
                    continue;
                }
                // Only the invocation whose `(` follows the macro name.
                let Some(open) = src[from..]
                    .find('(')
                    .filter(|offset| src[from..from + offset].chars().all(char::is_whitespace))
                else {
                    continue;
                };
                let arg = macro_arg(&src, from + open);
                if arg.contains("redact_credentials_for_output") {
                    continue;
                }
                let rendered = TERMINAL_CONTENT_ACCESSORS
                    .iter()
                    .find(|accessor| arg.contains(**accessor))
                    .map(|accessor| (*accessor).to_string())
                    .or_else(|| {
                        let regions = interpolated_regions(arg);
                        bindings
                            .iter()
                            .find(|binding| {
                                regions.iter().any(|region| mentions_ident(region, binding))
                            })
                            .map(|binding| format!("`{binding}`, bound from terminal content"))
                    });
                if let Some(what) = rendered {
                    let line = src[..start].matches('\n').count() + 1;
                    offenders.push(format!("{}:{line} — {name} renders {what}", path.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "terminal content is being written straight to captured test output, where the \
         redacting panic hook cannot reach it. A rendered API key suffix there lands in \
         nextest's run log and the raw JUnit report (issue #502/#785 audit S1). Wrap the \
         argument in `common::redact_credentials_for_output`:\n  {}",
        offenders.join("\n  ")
    );
}

/// Scenario: Scan every test source for a competing panic-hook
/// installation. The redacting seam is a process-global hook, so a test that
/// installs its own after it — even a `|_| {}` one to silence an expected
/// panic — silently removes the redaction for the rest of that binary's
/// run. The invariant is enforced here rather than left as a comment.
///
/// The needles are assembled with `concat!` so this scanner does not match
/// its own source, which is also why neither appears spelled out anywhere
/// in this function.
///
/// They match the CALL rather than a `panic::`-qualified path, because an
/// IMPORTED symbol installs a hook just as thoroughly as a qualified one and
/// a path-shaped needle never saw it — `use std::panic::set_hook;` followed
/// by a bare call was invisible to the predecessor of this scan. Matching on
/// the trailing `(` is what keeps a prose mention of the function in a
/// comment from being reported, and it is also why
/// `registry.set_hook_socket(…)` in `tests/daemon_protocol.rs` is correctly
/// ignored. The `use` arm is separate because an import has no call
/// parentheses to match on.
///
/// `src/` is deliberately not scanned: those hooks (`src/ui.rs`'s terminal
/// restore, and two in-crate unit tests) live in the deck BINARY or in the
/// lib's own test target, neither of which shares a process with this
/// harness.
#[test]
fn no_test_installs_a_competing_panic_hook() {
    let calls = [concat!("set_", "hook("), concat!("take_", "hook(")];
    let imported = |line: &str| {
        line.trim_start().starts_with("use ")
            && (line.contains(concat!("set_", "hook")) || line.contains(concat!("take_", "hook")))
    };
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut sources: Vec<PathBuf> = std::fs::read_dir(&tests_dir)
        .expect("read tests/")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "rs").then_some(path)
        })
        .collect();
    sources.push(tests_dir.join("common").join("mod.rs"));
    sources.sort();
    assert!(sources.len() > 50, "the scan is not reaching the suite");

    let mut offenders = Vec::new();
    let mut seam_installations = 0;
    for path in &sources {
        let src = std::fs::read_to_string(path).expect("read a test source");
        for (line_no, line) in src.lines().enumerate() {
            if !calls.iter().any(|needle| line.contains(needle)) && !imported(line) {
                continue;
            }
            // The seam's own installation is the one legitimate site.
            if path.ends_with("common/mod.rs") && line.contains("Box::new(|info|") {
                seam_installations += 1;
                continue;
            }
            offenders.push(format!(
                "{}:{} — {}",
                path.display(),
                line_no + 1,
                line.trim()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "a panic hook installed here replaces the credential-redacting seam for the rest \
         of this test binary's run, and a rendered API key would then reach nextest's \
         captured output and the raw JUnit report unredacted (issue #502/#785 audit S1). \
         The seam is a `Once`, so it cannot reinstall itself afterwards — rework the test \
         instead:\n  {}",
        offenders.join("\n  ")
    );
    // Non-vacuity: if the seam's own installation stopped matching, this
    // scan would be looking for something that no longer exists.
    assert_eq!(
        seam_installations, 1,
        "expected to find exactly the seam's own hook installation; the scanner has drifted \
         away from what it is meant to be guarding"
    );
}

/// Scenario: The approval decision reads the credential source the IMPORT
/// recorded, and that record cannot be revised once set — so one process
/// has one answer however the host changes underneath it.
///
/// Issue #785's non-blocking TOCTOU. The import and the seeding used to
/// re-derive the answer independently; a rotation landing between the two
/// reads made them disagree, and one direction of that disagreement moves a
/// developer's run off their subscription and onto metered billing.
#[test]
fn the_seeding_reads_the_credential_source_the_import_recorded() {
    // Order-independent: whichever value this process settled on first is
    // the one both halves must keep seeing.
    let recorded = *CLAUDE_OAUTH_SEEDED.get_or_init(|| false);
    assert_eq!(claude_run_is_oauth_backed(), recorded);
    assert!(
        CLAUDE_OAUTH_SEEDED.set(!recorded).is_err(),
        "the recorded credential source must not be revisable"
    );
    assert_eq!(claude_run_is_oauth_backed(), recorded);
}

/// Scenario: Seed Claude Code's API-key approval into a config on a host
/// whose OAuth credential set is UNUSABLE and that records NO answer for
/// this key — a key-only host with a fresh HOME, which since #502 means a
/// developer machine that has never run `claude login` rather than a
/// runner. The key is approved, because it is the only way in and there is
/// no stored decision to respect.
#[test]
fn a_key_authorised_run_approves_a_key_the_host_never_answered_for() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let id = claude_api_key_response_id(key);
    let mut cfg = serde_json::json!({ "hasCompletedOnboarding": true });
    seed_claude_api_key_response(&mut cfg, key, false, false);
    assert!(response_list_contains(&cfg, "approved", &id));
    assert!(!response_list_contains(&cfg, "rejected", &id));
}

/// Scenario: The host recorded a REJECTION for this exact key and OAuth is
/// unusable — a key-only developer machine where the "No" is a real human
/// decision. Nothing authorises overriding it, so the refusal stands and
/// the key is NOT approved. `check_claude_available` refuses the run for
/// the same reason, so the developer gets a named skip rather than a
/// silent bill.
///
/// The predecessor of this test asserted the opposite, on the justification
/// that a runner would otherwise inherit a developer's refusal. That does
/// not hold: `ubuntu-latest` starts with a fresh HOME and nothing copies
/// `~/.claude.json` onto it, so the removal was a no-op in CI and took
/// effect only here — the one place it overrode a human.
#[test]
fn a_stored_local_refusal_is_respected_when_nothing_authorises_an_override() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let id = claude_api_key_response_id(key);
    let mut cfg = serde_json::json!({
        "hasCompletedOnboarding": true,
        "customApiKeyResponses": { "approved": [], "rejected": [id.clone()] },
    });
    seed_claude_api_key_response(&mut cfg, key, false, false);
    assert!(!response_list_contains(&cfg, "approved", &id));
    assert!(response_list_contains(&cfg, "rejected", &id));
}

/// Scenario: The same stored refusal, but the run carries the explicit
/// require-real authorisation. The refusal is overridden and the key
/// approved — the override is something a caller asked for rather than
/// something the harness did on its own.
#[test]
fn an_authorised_run_overrides_a_stored_refusal() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let id = claude_api_key_response_id(key);
    let mut cfg = serde_json::json!({
        "customApiKeyResponses": { "approved": [], "rejected": [id.clone()] },
    });
    seed_claude_api_key_response(&mut cfg, key, false, true);
    assert!(response_list_contains(&cfg, "approved", &id));
    assert!(!response_list_contains(&cfg, "rejected", &id));
}

/// Scenario: Seed the answer on a host whose OAuth credential set IS
/// usable — a developer's machine. The key is REJECTED rather than
/// approved, so the imported credential set stays authoritative and the run
/// does not quietly move off the developer's subscription onto metered API
/// billing. Measured: with the key rejected and no credential file, Claude
/// Code declines the key and falls through to its login prompt, so on an
/// OAuth host it authenticates exactly as it did before this existed.
#[test]
fn an_oauth_authorised_run_rejects_the_ambient_key_instead_of_approving_it() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let id = claude_api_key_response_id(key);
    let mut cfg = serde_json::json!({ "hasCompletedOnboarding": true });
    seed_claude_api_key_response(&mut cfg, key, true, false);
    assert!(response_list_contains(&cfg, "rejected", &id));
    assert!(!response_list_contains(&cfg, "approved", &id));
}

/// Scenario: The host config already approves this key and the host also has
/// a usable OAuth credential set. The approval is REVOKED inside the
/// isolated test HOME, because an approved key beats a usable OAuth file.
///
/// Measured on claude 2.1.252 with a real credential set (the reviewer's
/// note A): OAuth file present + key exported + key approved renders
/// `Haiku 4.5 · API Usage Billing`, and the identical HOME with the key
/// rejected renders `Haiku 4.5 · Claude Team`. The host approval was
/// recorded for the developer's own interactive sessions, at a time when
/// the key never reached a test agent at all; honouring it here would newly
/// bill them for a test run, while overriding it costs them nothing.
#[test]
fn an_oauth_host_revokes_an_inherited_approval_rather_than_paying_for_it() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let id = claude_api_key_response_id(key);
    let mut cfg = serde_json::json!({
        "customApiKeyResponses": { "approved": [id.clone()], "rejected": [] },
    });
    seed_claude_api_key_response(&mut cfg, key, true, false);
    assert!(!response_list_contains(&cfg, "approved", &id));
    assert!(response_list_contains(&cfg, "rejected", &id));
}

/// Scenario: Seed an API key of 20 characters or fewer, where the "last 20
/// characters" derivative IS the whole key. Nothing is written at all — not
/// the key, not a response block — so the claim that the raw key never
/// appears in `~/.claude.json` stays true for every input rather than only
/// for realistic ones.
///
/// Each fixture asserts its own length, because the boundary is the whole
/// point: an earlier draft of this test used a "twenty-character" fixture
/// that was actually 21, which passed for the wrong reason (the id was a
/// strict suffix, so the full string genuinely was absent) and exercised
/// the refusal not at all.
#[test]
fn a_key_shorter_than_its_own_response_id_is_never_written_into_the_config() {
    // 12, 20, and 20 counted in CHARACTERS rather than bytes — the id is
    // built from `chars()`, so a multibyte value at the boundary has to
    // land on the same side of it.
    for key in [
        "sk-ant-short",
        "exactly-twenty-chars",
        "sk-\u{e9}\u{e9}-twenty-charsXY",
    ] {
        assert!(
            key.chars().count() <= 20,
            "fixture {key:?} is {} characters, so it does not exercise the refusal",
            key.chars().count()
        );
        let mut cfg = serde_json::json!({ "hasCompletedOnboarding": true });
        seed_claude_api_key_response(&mut cfg, key, false, true);
        let rendered = cfg.to_string();
        assert!(
            !rendered.contains(key),
            "the key itself was written into the config: {rendered}"
        );
        assert!(
            cfg.get("customApiKeyResponses").is_none(),
            "a response block was built for a key that must not be recorded \
             at all: {rendered}"
        );
    }
    // The boundary in the other direction: 21 characters has a strict
    // 20-character suffix, so it IS seeded.
    let key = "twenty-one-characters";
    assert_eq!(key.chars().count(), 21);
    let mut cfg = serde_json::json!({});
    seed_claude_api_key_response(&mut cfg, key, false, true);
    let id = claude_api_key_response_id(key);
    assert_eq!(id.chars().count(), 20);
    assert!(response_list_contains(&cfg, "approved", &id));
    assert!(
        !cfg.to_string().contains(key),
        "the whole key reached the config even though its id is a strict suffix"
    );
}

/// Scenario: Seed into a host config whose `customApiKeyResponses` is
/// missing entirely, or is present but the wrong JSON type. Neither shape
/// may panic or silently drop the answer — a config Claude Code has never
/// written the key into is the ordinary first-run case.
#[test]
fn a_missing_or_malformed_response_block_is_rebuilt_rather_than_trusted() {
    let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
    let id = claude_api_key_response_id(key);
    for hostile in [
        serde_json::json!({}),
        serde_json::json!({ "customApiKeyResponses": 7 }),
        serde_json::json!({ "customApiKeyResponses": { "approved": "not-a-list" } }),
    ] {
        let mut cfg = hostile;
        seed_claude_api_key_response(&mut cfg, key, false, false);
        assert!(
            response_list_contains(&cfg, "approved", &id),
            "the approval was lost: {cfg}"
        );
    }
}

/// Scenario: Ask whether an ambient API key is usable. Unset, empty and
/// whitespace-only all mean absent — the same rule the three
/// `check_pi_available` copies apply — while a real value comes back
/// VERBATIM, because
/// verbatim is what the spawned agent receives.
#[test]
fn an_empty_or_whitespace_only_api_key_counts_as_absent() {
    let prev = std::env::var_os(ANTHROPIC_API_KEY_ENV);
    // SAFETY: nextest runs one test per process, so this is single-threaded;
    // the var is restored before returning.
    unsafe { std::env::remove_var(ANTHROPIC_API_KEY_ENV) };
    assert!(anthropic_api_key().is_none(), "unset must read as absent");
    unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, "") };
    assert!(anthropic_api_key().is_none(), "empty must read as absent");
    unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, " \t\n") };
    assert!(
        anthropic_api_key().is_none(),
        "whitespace-only must read as absent"
    );
    unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, "sk-ant-fake") };
    assert_eq!(anthropic_api_key().as_deref(), Some("sk-ant-fake"));
    match prev {
        Some(value) => unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, value) },
        None => unsafe { std::env::remove_var(ANTHROPIC_API_KEY_ENV) },
    }
}

/// Scenario: Ask whether an ambient Anthropic key is enough to run the
/// OpenCode tests. It is only enough when the configured test model names
/// the `anthropic` provider — the harness forwards that key and no other,
/// so opening the gate for an `openai/...` model would turn a clean skip
/// into a failure deep in a PTY wait.
#[test]
fn the_opencode_env_key_path_is_offered_only_for_an_anthropic_model() {
    assert_eq!(
        OPENCODE_TEST_MODEL_DEFAULT.split_once('/').map(|(p, _)| p),
        Some("openai"),
        "the default model is the case the provider match exists to exclude"
    );
    // `opencode_test_model` memoises, so drive the pure predicate the gate
    // uses rather than the OnceLock: provider match AND key presence.
    for (model, provider_matches) in [
        ("anthropic/claude-haiku-4-5", true),
        ("openai/gpt-5.4-mini", false),
        ("openrouter/anthropic/claude-haiku-4-5", false),
        ("no-slash-at-all", false),
    ] {
        assert_eq!(
            model.split_once('/').is_some_and(|(p, _)| p == "anthropic"),
            provider_matches,
            "provider match for {model}"
        );
    }
}
