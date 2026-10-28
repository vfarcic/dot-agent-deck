//! Issue #732: the trust policy [`super::durable_binary_path`] applies to every
//! path it pins into an agent's config — owner, every directory the kernel
//! walks to reach the file, and every symlink on the way.
//!
//! Each case drives [`super::durable_binary_resolution_judged`] with an
//! injected [`super::TrustedWriters`], so whether a group counts as trusted is
//! the fixture's choice rather than the box's umask and group layout. The
//! filesystem is real: the fixtures make real files, directories and symlinks
//! and set their modes, and the walk reads them back.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::{
    DEFAULT_BINARY_NAME, ResolutionArm, TrustedWriters, durable_binary_resolution_judged,
    is_private_group, untrusted_writer,
};

fn name() -> String {
    format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX)
}

fn me() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The current user and root, with no group trusted beyond gid 0 — so any
/// group-writable entry the fixture makes is one another account could write.
fn no_group_trusted() -> TrustedWriters {
    TrustedWriters {
        uid: me(),
        groups: vec![0],
    }
}

/// The same writers, also trusting `gid` — the shape the user's private group
/// gets in production.
fn trusting_group(gid: u32) -> TrustedWriters {
    TrustedWriters {
        uid: me(),
        groups: vec![0, gid],
    }
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {mode:o} {}: {e}", path.display()));
}

/// A scratch root whose every directory the fixture creates is `0755`, so a
/// `umask 002` box makes the same tree a `umask 022` one does. Directories the
/// fixture later loosens are loosened explicitly.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = crate::test_temp::tempdir().expect("trust fixture tempdir");
        let root = dir.path().to_path_buf();
        let fixture = Self {
            home: root.join("home"),
            root,
            _dir: dir,
        };
        // `tempfile` creates it with the process umask applied, so `0775`
        // on a `umask 002` box.
        chmod(&fixture.root, 0o755);
        fixture.mkdir(&fixture.home);
        fixture
    }

    /// `dir` and its missing parents under the root, each `0755`.
    fn mkdir(&self, dir: &Path) {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
        let mut current = dir;
        while current != self.root {
            chmod(current, 0o755);
            current = current.parent().expect("under the fixture root");
        }
    }

    /// A real `0755` executable at `path`, parents created `0755`.
    fn executable(&self, path: &Path) -> PathBuf {
        self.mkdir(path.parent().expect("candidate has a parent"));
        crate::test_isolation::write_script(path, b"#!/bin/sh\nexit 0\n").expect("write candidate");
        chmod(path, 0o755);
        path.to_path_buf()
    }

    fn installed(&self) -> PathBuf {
        self.home.join(".local").join("bin").join(name())
    }

    /// A running deck nothing vouches for: not an artifact, not installed.
    fn scratch_exe(&self) -> PathBuf {
        self.executable(&self.root.join("scratch").join(name()))
    }

    /// A cargo artifact as the running binary, so resolution has to find an
    /// install and the last resort is refused regardless.
    fn artifact_exe(&self) -> PathBuf {
        self.executable(
            &self
                .root
                .join("checkout")
                .join("target")
                .join("debug")
                .join(name()),
        )
    }
}

/// Give `path` a group no [`no_group_trusted`] writer is in, which means any
/// group but gid 0. On Linux a new entry already has the creator's primary
/// group, which is gid 0 only when the test runs as root. On macOS it takes
/// its directory's group, `wheel` (gid 0) under `/var/tmp`, so a non-root run
/// hands it to its own primary group; root hands it to `nogroup`.
fn untrusted_group(path: &Path) {
    if gid_of(path) == 0 {
        // SAFETY: `getegid` has no preconditions and cannot fail.
        let gid = if me() == 0 {
            65534
        } else {
            unsafe { libc::getegid() }
        };
        std::os::unix::fs::chown(path, None, Some(gid))
            .unwrap_or_else(|e| panic!("chgrp {gid} {}: {e}", path.display()));
    }
    assert_ne!(gid_of(path), 0, "{} still has gid 0", path.display());
}

fn gid_of(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).expect("stat fixture").gid()
}

fn path_of(dirs: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(dirs).expect("join synthetic PATH")
}

/// PRD #381 audit, HIGH, moved here from `paths.rs` when issue #732 replaced
/// the file-mode-only check: a `~/.local/bin` candidate a group outside the
/// trusted writers can rewrite is skipped, and the next candidate wins.
#[test]
fn a_candidate_an_untrusted_group_can_write_is_skipped() {
    let fixture = Fixture::new();
    let loose = fixture.executable(&fixture.installed());
    chmod(&loose, 0o775);
    untrusted_group(&loose);
    let tight_dir = fixture.root.join("opt").join("bin");
    let tight = fixture.executable(&tight_dir.join(name()));

    let resolved = durable_binary_resolution_judged(
        Ok(fixture.artifact_exe()),
        &fixture.home,
        Some(&path_of(&[&tight_dir])),
        &no_group_trusted(),
    )
    .expect("the tight $PATH candidate resolves");

    assert_eq!(resolved.path, tight.to_str().expect("UTF-8"));
    assert_eq!(resolved.arm, ResolutionArm::Install);
}

/// The other side of the same line, and the Debian/Ubuntu default: under
/// `umask 002` `cargo install` copies a `0775` binary owned by the user's
/// private group into `~/.cargo/bin`. That group is a trusted writer, so the
/// install is pinned rather than passed over.
#[test]
fn a_candidate_only_a_trusted_group_can_write_is_pinned() {
    let fixture = Fixture::new();
    let installed = fixture.executable(&fixture.installed());
    chmod(&installed, 0o775);
    chmod(installed.parent().expect("bin dir"), 0o775);

    let resolved = durable_binary_resolution_judged(
        Ok(fixture.artifact_exe()),
        &fixture.home,
        None,
        &trusting_group(gid_of(&installed)),
    )
    .expect("a private-group-writable install resolves");

    assert_eq!(resolved.path, installed.to_str().expect("UTF-8"));
}

/// The ancestor half, which the file-mode check could not see: a candidate
/// whose own mode is `0755` but whose directory an untrusted group can write
/// can be replaced by that group, so it is skipped.
#[test]
fn a_candidate_in_a_directory_an_untrusted_group_can_write_is_skipped() {
    let fixture = Fixture::new();
    let shared_dir = fixture.root.join("srv").join("shared-bin");
    fixture.executable(&shared_dir.join(name()));
    chmod(&shared_dir, 0o775);
    untrusted_group(&shared_dir);
    let tight_dir = fixture.root.join("usr").join("bin");
    let tight = fixture.executable(&tight_dir.join(name()));

    let resolved = durable_binary_resolution_judged(
        Ok(fixture.artifact_exe()),
        &fixture.home,
        Some(&path_of(&[&shared_dir, &tight_dir])),
        &no_group_trusted(),
    )
    .expect("the later trusted candidate resolves");

    assert_eq!(resolved.path, tight.to_str().expect("UTF-8"));
}

/// A higher ancestor counts as much as the parent: renaming any directory on
/// the way replaces everything beneath it.
#[test]
fn a_world_writable_grandparent_directory_disqualifies_a_candidate() {
    let fixture = Fixture::new();
    let shared = fixture.root.join("shared");
    let candidate = fixture.executable(&shared.join("bin").join(name()));
    chmod(&shared, 0o777);

    let reason = untrusted_writer(&candidate, &no_group_trusted())
        .expect("a world-writable ancestor is untrusted");
    assert!(
        reason.contains(shared.to_str().expect("UTF-8")) && reason.contains("every account"),
        "the reason names the directory and why: {reason}"
    );
}

/// The sticky exception — `/tmp`, `/var/tmp`, `/nix/store`: in a sticky
/// directory only an entry's owner can rename or remove it, so a world- or
/// group-writable sticky ancestor does not disqualify an entry the user owns.
#[test]
fn a_sticky_world_writable_ancestor_does_not_disqualify_a_candidate() {
    let fixture = Fixture::new();
    let sticky = fixture.root.join("sticky");
    let candidate = fixture.executable(&sticky.join("mine").join(name()));
    chmod(&sticky, 0o1777);

    assert_eq!(untrusted_writer(&candidate, &no_group_trusted()), None);
}

/// The canonical-target half of the issue: a `~/.local/bin` symlink is still
/// written as the link, but a target another account can replace makes the
/// link no safer than the target, so it is skipped.
#[test]
fn a_symlink_whose_target_an_untrusted_writer_can_replace_is_skipped() {
    let fixture = Fixture::new();
    let shared_dir = fixture.root.join("shared-build");
    let target = fixture.executable(&shared_dir.join(name()));
    chmod(&shared_dir, 0o777);
    let link = fixture.installed();
    fixture.mkdir(link.parent().expect("bin dir"));
    std::os::unix::fs::symlink(&target, &link).expect("symlink the installed name");

    let err = durable_binary_resolution_judged(
        Ok(fixture.artifact_exe()),
        &fixture.home,
        None,
        &no_group_trusted(),
    )
    .expect_err("nothing trusted is left, and the artifact is refused");

    assert!(
        err.contains("Passed over") && err.contains(link.to_str().expect("UTF-8")),
        "the refusal names the install it passed over: {err}"
    );
    assert!(
        err.contains(shared_dir.to_str().expect("UTF-8")),
        "and the directory that disqualified it: {err}"
    );
}

/// The symlink itself is judged by its own directory and owner, and a relative
/// target with `..` is resolved from the link's directory as the kernel would.
#[test]
fn a_relative_symlink_target_is_resolved_from_the_link_directory() {
    let fixture = Fixture::new();
    let keg = fixture.executable(
        &fixture
            .root
            .join("prefix")
            .join("Cellar")
            .join("1.0")
            .join(name()),
    );
    let bin = fixture.root.join("prefix").join("bin");
    fixture.mkdir(&bin);
    let link = bin.join(name());
    std::os::unix::fs::symlink(
        Path::new("..").join("Cellar").join("1.0").join(name()),
        &link,
    )
    .expect("relative symlink, as Homebrew makes");

    assert_eq!(untrusted_writer(&link, &no_group_trusted()), None);

    chmod(keg.parent().expect("keg dir"), 0o777);
    let reason = untrusted_writer(&link, &no_group_trusted())
        .expect("the keg directory reached through `..` is judged");
    assert!(reason.contains("Cellar"), "{reason}");
}

/// A symlink loop is untrusted rather than a hang.
#[test]
fn a_symlink_loop_is_untrusted() {
    let fixture = Fixture::new();
    let dir = fixture.root.join("loop");
    fixture.mkdir(&dir);
    std::os::unix::fs::symlink(dir.join("b"), dir.join("a")).expect("a -> b");
    std::os::unix::fs::symlink(dir.join("a"), dir.join("b")).expect("b -> a");

    let reason = untrusted_writer(&dir.join("a"), &no_group_trusted()).expect("a loop");
    assert!(reason.contains("too many symlinks"), "{reason}");
}

/// The owner half. A test cannot create a file owned by another account
/// without root, so unprivileged it names another account as the user instead:
/// every entry the fixture made then "belongs to another account". As root it
/// hands the file to `nobody`.
#[test]
fn a_candidate_owned_by_another_account_is_untrusted() {
    let fixture = Fixture::new();
    let candidate = fixture.scratch_exe();
    let writers = if me() == 0 {
        std::os::unix::fs::chown(&candidate, Some(65534), None).expect("chown as root");
        no_group_trusted()
    } else {
        TrustedWriters {
            uid: me().wrapping_add(1),
            groups: vec![0],
        }
    };

    let reason = untrusted_writer(&candidate, &writers).expect("not ours");
    assert!(reason.contains("belongs to another account"), "{reason}");
}

/// Step 3 is held to the policy too, and there failing it is a refusal, not a
/// skip: the running binary is no exemption, because a pin runs it at every
/// hook from now on.
#[test]
fn the_last_resort_is_refused_when_another_account_can_replace_it() {
    let fixture = Fixture::new();
    let exe = fixture.scratch_exe();
    chmod(exe.parent().expect("scratch dir"), 0o777);

    let err =
        durable_binary_resolution_judged(Ok(exe.clone()), &fixture.home, None, &no_group_trusted())
            .expect_err("an untrusted last resort is refused");

    assert!(
        err.starts_with(&format!("refusing to write `{}`", exe.display())),
        "{err}"
    );
    assert!(err.contains("every account"), "says why: {err}");
    assert!(err.contains("chmod go-w"), "says what to do: {err}");
    assert!(
        !err.contains("Passed over"),
        "the path the message already names is not listed again: {err}"
    );
}

/// And the same last resort, trusted, is still pinned — `hooks/install/008`'s
/// scratch copy that is the only deck on the machine.
#[test]
fn a_trusted_last_resort_is_still_pinned() {
    let fixture = Fixture::new();
    let exe = fixture.scratch_exe();

    let resolved =
        durable_binary_resolution_judged(Ok(exe.clone()), &fixture.home, None, &no_group_trusted())
            .expect("a trusted last resort resolves");

    assert_eq!(resolved.path, exe.to_str().expect("UTF-8"));
    assert_eq!(resolved.arm, ResolutionArm::LastResort);
}

/// Step 1 is judged as well: an installed running binary another account can
/// replace is passed over for a trusted install of the same name.
#[test]
fn an_untrusted_running_install_is_passed_over_for_a_trusted_one() {
    let fixture = Fixture::new();
    let loose_dir = fixture.root.join("loose").join("bin");
    let running = fixture.executable(&loose_dir.join(name()));
    chmod(&loose_dir, 0o777);
    let installed = fixture.executable(&fixture.installed());

    let resolved = durable_binary_resolution_judged(
        Ok(running),
        &fixture.home,
        Some(&path_of(&[&loose_dir])),
        &no_group_trusted(),
    )
    .expect("the trusted ~/.local/bin install resolves");

    assert_eq!(resolved.path, installed.to_str().expect("UTF-8"));
    assert_eq!(resolved.arm, ResolutionArm::Install);
}

/// Production's writers always include the effective user and root's group.
#[test]
fn the_current_writers_trust_this_user_and_gid_zero() {
    let writers = TrustedWriters::current();
    assert_eq!(writers.uid, me());
    assert!(writers.groups.contains(&0));
}

/// The private-group decision itself, on supplied account records, so a
/// mistake in it cannot hide behind the box's own groups. Accepted: the
/// Debian/Ubuntu shape. Refused: every way another account can be in the group.
#[test]
fn the_private_group_decision_accepts_only_a_group_of_one() {
    let alice = b"alice".as_slice();
    let uid = 1000;
    // (case, group name, supplementary members, primary-group uids, private?)
    type Case<'a> = (&'a str, &'a [u8], &'a [&'a [u8]], &'a [u32], bool);
    let cases: &[Case] = &[
        (
            "named after the user, nobody else",
            b"alice",
            &[],
            &[1000],
            true,
        ),
        (
            "the user listed as its own member",
            b"alice",
            &[b"alice"],
            &[1000],
            true,
        ),
        (
            "root's primary group is it too",
            b"alice",
            &[],
            &[1000, 0],
            true,
        ),
        (
            "a shared primary group (staff, users)",
            b"staff",
            &[],
            &[1000],
            false,
        ),
        (
            "another supplementary member",
            b"alice",
            &[b"alice", b"www-data"],
            &[1000],
            false,
        ),
        (
            "another account's primary group",
            b"alice",
            &[],
            &[1000, 1001],
            false,
        ),
    ];
    for (case, group, members, primary_uids, expected) in cases {
        let members: Vec<Vec<u8>> = members.iter().map(|m| m.to_vec()).collect();
        assert_eq!(
            is_private_group(alice, uid, group, &members, primary_uids),
            *expected,
            "{case}"
        );
    }
}
