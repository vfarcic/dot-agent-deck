#![cfg(all(feature = "e2e", target_os = "linux"))]

//! M47n, measured (PRD #1666 part 3, U4): the export path rule's verdicts
//! (`composer_probe::path_rule::verify_export` through the real Linux view)
//! against what a second account can actually do to each fixture.
//!
//! The test builds each fixture as the user under a fresh directory in
//! `/var/tmp` and asks the rule about its export. A disposable container
//! (`docker run --network none --pull never`, the image already on the host)
//! then acts as another uid, 1001, with its own group and again with the
//! user's primary group, over a bind mount of the same directory: it writes
//! to the export, creates an entry beside it, renames one of its own files
//! in, and renames one over the export. No container runs as the user and
//! none as root.
//!
//! Every fixture the rule accepts must resist every attempt; every one it
//! refuses must have at least one attempt that succeeds as either account,
//! apart from the hard-link fixture, which the rule refuses for a second
//! path it cannot see the reach of (the attempts are printed). Where Docker
//! or the image isn't available, or `/` or `/var/tmp` isn't on an
//! allow-listed filesystem, the test skips.

mod common;

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;

use dot_agent_deck::composer_probe::path_rule::{ALLOWED_FILESYSTEMS, verify_export};
use dot_agent_deck::platform::fsperm::fs_view::LinuxFsView;
use spec::spec;

const IMAGE: &str = "ubuntu:24.04";
const OTHER: u32 = 1001;

const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const MASK: u16 = 0x10;
const OTHER_ENTRY: u16 = 0x20;
const UNDEFINED_ID: u32 = u32::MAX;

fn uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() }
}

fn gid() -> u32 {
    // SAFETY: `getegid` has no preconditions.
    unsafe { libc::getegid() }
}

// `f_type`'s width varies by architecture.
#[allow(clippy::unnecessary_cast)]
fn allow_listed(path: &Path) -> bool {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `statfs` is plain data, so all-zero is a valid value.
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a writable `statfs`.
    assert_eq!(unsafe { libc::statfs(c.as_ptr(), &mut info) }, 0);
    ALLOWED_FILESYSTEMS
        .iter()
        .any(|&(allowed, _)| allowed == info.f_type as u32)
}

fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut value = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        value.extend_from_slice(&tag.to_le_bytes());
        value.extend_from_slice(&perm.to_le_bytes());
        value.extend_from_slice(&id.to_le_bytes());
    }
    value
}

fn set_acl(path: &Path, value: &[u8]) -> std::io::Result<()> {
    let p = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated path and name; `value` is a live buffer.
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A grant to `OTHER` of `perm`, behind a mask of `perm`, on a 0755 node.
fn grant(perm: u16) -> Vec<u8> {
    acl(&[
        (USER_OBJ, 7, UNDEFINED_ID),
        (USER, perm, OTHER),
        (GROUP_OBJ, 5, UNDEFINED_ID),
        (MASK, perm, UNDEFINED_ID),
        (OTHER_ENTRY, 5, UNDEFINED_ID),
    ])
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn dir(path: &Path, mode: u32) {
    fs::create_dir(path).unwrap();
    chmod(path, mode);
}

fn export(path: &Path, mode: u32) {
    fs::write(path, "the box\n").unwrap();
    chmod(path, mode);
}

/// (name, the export's path relative to the root, whether the rule should
/// accept it). Each is built fresh under `root`.
fn build(root: &Path) -> Vec<(&'static str, &'static str, bool)> {
    let r = |p: &str| root.join(p);
    dir(&r("p1"), 0o700);
    export(&r("p1/export"), 0o600);
    dir(&r("p2"), 0o700);
    export(&r("p2/export"), 0o664);
    dir(&r("p3"), 0o755);
    export(&r("p3/export"), 0o644);
    dir(&r("p4"), 0o755);
    dir(&r("p4real"), 0o700);
    export(&r("p4real/export"), 0o600);
    symlink("../p4real", r("p4/ln")).unwrap();
    dir(&r("p5"), 0o755);
    export(&r("p5/export"), 0o644);
    set_acl(&r("p5"), &grant(5)).unwrap();
    dir(&r("n1"), 0o775);
    export(&r("n1/export"), 0o600);
    dir(&r("n2"), 0o757);
    export(&r("n2/export"), 0o600);
    dir(&r("n3"), 0o1777);
    export(&r("n3/export"), 0o600);
    dir(&r("n4"), 0o755);
    export(&r("n4/export"), 0o666);
    dir(&r("n5"), 0o755);
    export(&r("n5/export"), 0o664);
    dir(&r("n6"), 0o755);
    export(&r("n6/export"), 0o600);
    set_acl(&r("n6"), &grant(7)).unwrap();
    dir(&r("n7"), 0o755);
    export(&r("n7/export"), 0o600);
    set_acl(&r("n7/export"), &grant(6)).unwrap();
    dir(&r("n8"), 0o755);
    export(&r("n8/export"), 0o644);
    fs::hard_link(r("n8/export"), r("n8/second")).unwrap();
    // The other account's own files, on the same filesystem.
    dir(&r("stage"), 0o1777);
    vec![
        ("p1-private-dir", "p1/export", true),
        ("p2-0664-in-0700", "p2/export", true),
        ("p3-0644-in-0755", "p3/export", true),
        ("p4-safe-symlink", "p4/ln/export", true),
        ("p5-acl-read-execute", "p5/export", true),
        ("n1-group-writable-dir", "n1/export", false),
        ("n2-other-writable-dir", "n2/export", false),
        ("n3-user-sticky-dir", "n3/export", false),
        ("n4-other-writable-file", "n4/export", false),
        ("n5-group-writable-file", "n5/export", false),
        ("n6-acl-write-on-dir", "n6/export", false),
        ("n7-acl-write-on-file", "n7/export", false),
        ("n8-second-hard-link", "n8/export", false),
    ]
}

/// The attempts, as the other account, on the export at `/m/<path>`: one
/// line per attempt, `<attempt> <exit status>`.
const ATTEMPTS: &str = r#"
set -u
e="/m/$1"; d=$(dirname "$e")
echo B > /m/stage/src1 2>/dev/null; echo B > /m/stage/src2 2>/dev/null
sh -c "echo B >> '$e'" 2>/dev/null; echo "write $?"
touch "$d/b-new" 2>/dev/null; echo "create $?"
mv /m/stage/src1 "$d/b-renamed" 2>/dev/null; echo "rename-in $?"
mv /m/stage/src2 "$e" 2>/dev/null; echo "replace $?"
id -u
"#;

fn attempts(root: &Path, path: &str, group: u32) -> Result<Vec<(String, bool)>, String> {
    let output = std::process::Command::new("docker")
        .args(["run", "--rm", "--pull", "never", "--network", "none"])
        .arg("--user")
        .arg(format!("{OTHER}:{group}"))
        .arg("-v")
        .arg(format!("{}:/m", root.display()))
        .args([IMAGE, "sh", "-c", ATTEMPTS, "attempts", path])
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.pop(), Some(OTHER.to_string().as_str()), "{text}");
    Ok(lines
        .into_iter()
        .map(|line| {
            let (name, rc) = line.split_once(' ').unwrap();
            (name.to_string(), rc == "0")
        })
        .collect())
}

fn preflight() -> Result<(), String> {
    for at in ["/", "/var/tmp"] {
        if !allow_listed(Path::new(at)) {
            return Err(format!("{at} is not on an allow-listed filesystem"));
        }
    }
    let image = std::process::Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .output()
        .map_err(|e| format!("docker is not available: {e}"))?;
    if !image.status.success() {
        return Err(format!("the {IMAGE} image is not on this host (no pull)"));
    }
    if uid() == OTHER {
        return Err(format!("the test's own uid is {OTHER}"));
    }
    Ok(())
}

/// Scenario: As the user, build thirteen export fixtures under a fresh directory in `/var/tmp` (private, 0644 in 0755, Claude's 0664 in 0700, a safe symlink, a read-and-execute ACL grant; group-writable, other-writable and user-sticky directories, group- and other-writable files, ACL write grants on the directory and the file, a second hard link) and ask the deck's export rule about each. Then, from a container with no network, as uid 1001 with its own group and again with the user's group, try to write the export, create an entry beside it, rename a file in and rename one over it: everything the rule accepts resists every attempt, and everything it refuses except the hard link gives way to at least one.
#[spec("inbox/probe/096")]
#[test]
fn probe_096_the_export_rule_against_a_second_account() {
    if common::_skip_if_err(preflight()) {
        return;
    }
    let mut rows = Vec::new();
    for group in [OTHER, gid()] {
        let root = tempfile::Builder::new()
            .prefix("dad-m47n-")
            .tempdir_in("/var/tmp")
            .unwrap();
        chmod(root.path(), 0o755);
        for (name, path, expect) in build(root.path()) {
            let verdict = verify_export(&mut LinuxFsView, &root.path().join(path), uid());
            let accepted = verdict.is_ok();
            assert_eq!(
                accepted,
                expect,
                "{name}: the rule said {:?}",
                verdict.err()
            );
            let tried = attempts(root.path(), path, group).unwrap();
            let gave_way: Vec<&str> = tried
                .iter()
                .filter(|(_, ok)| *ok)
                .map(|(n, _)| n.as_str())
                .collect();
            eprintln!(
                "M47N {name} group={} accepted={accepted} refusal={:?} gave_way={gave_way:?}",
                if group == OTHER { "own" } else { "user's" },
                verdict.as_ref().err().map(|r| (&r.at, &r.cause)),
            );
            rows.push((name, group, accepted, gave_way.len()));
            if accepted {
                assert!(
                    gave_way.is_empty(),
                    "{name}: accepted, yet {gave_way:?} succeeded"
                );
            }
        }
    }
    for (name, _, accepted, _) in rows.iter().filter(|r| !r.2 && r.1 == OTHER) {
        let any = rows.iter().filter(|r| r.0 == *name).any(|r| r.3 > 0);
        if *name != "n8-second-hard-link" {
            assert!(
                any,
                "{name}: refused (accepted={accepted}), yet no attempt succeeded as either account"
            );
        }
    }
}
