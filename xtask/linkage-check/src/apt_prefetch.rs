//! `.github/scripts/apt-prefetch.sh` downloads the packages `desktop-driver`
//! and `desktop-browser` install, with a time limit on each attempt, and moves
//! to the next mirror in the runner's list after an attempt that does not
//! finish. It used to move only off `azure.archive.ubuntu.com`. On 2026-11-18
//! the `ubuntu-26.04` image listed `us-east-2.ec2.archive.ubuntu.com` first,
//! that mirror trickled, all three attempts stayed on it, and `desktop-driver`
//! went red on `main` (run 44262375554) with `archive.ubuntu.com` unused.
//!
//! These tests run the script under `bash` with stand-ins on `PATH`: `sudo`
//! runs its command as is, `apt-get` succeeds, and `timeout` records which
//! mirror headed the list when it was called and exits with the next code
//! from `FAKE_OUTCOMES` (124 is a timed-out attempt, 0 a finished one). The
//! mirror list is a temporary file passed in `APT_PREFETCH_MIRRORS`. No
//! network, no root.
//!
//! Unix-only, like `ci_unacquired_runners`: the script is bash, and its callers
//! are Ubuntu jobs.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .join(".github/scripts/apt-prefetch.sh")
}

const FAKE_SUDO: &str = "#!/usr/bin/env bash\nexec \"$@\"\n";

const FAKE_APT_GET: &str = "#!/usr/bin/env bash\nexit 0\n";

/// Logs the head of the mirror list (or `-` when there is none) and exits with
/// the next of `FAKE_OUTCOMES`.
const FAKE_TIMEOUT: &str = r#"#!/usr/bin/env bash
n=$(( $(cat "$FAKE_DIR/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$FAKE_DIR/count"
if [ -f "$APT_PREFETCH_MIRRORS" ]; then
  head -n 1 "$APT_PREFETCH_MIRRORS" | cut -f1 >> "$FAKE_DIR/used"
else
  echo - >> "$FAKE_DIR/used"
fi
set -- $FAKE_OUTCOMES
exit "${!n:-0}"
"#;

const EC2: &str = "http://us-east-2.ec2.archive.ubuntu.com/ubuntu/";
const AZURE: &str = "http://azure.archive.ubuntu.com/ubuntu/";
const ARCHIVE: &str = "https://archive.ubuntu.com/ubuntu/";
const SECURITY: &str = "https://security.ubuntu.com/ubuntu/";

struct Run {
    dir: tempfile::TempDir,
    out: Output,
}

impl Run {
    fn used(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("used"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn mirrors_left(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("mirrors"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.split('\t').next().unwrap_or("").to_string())
            .collect()
    }

    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out.stdout).into_owned()
    }
}

/// Runs the script against `mirrors` (one URL per line, `None` for no list)
/// with the attempt outcomes given.
fn run(mirrors: Option<&[&str]>, outcomes: &str) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, body) in [
        ("sudo", FAKE_SUDO),
        ("apt-get", FAKE_APT_GET),
        ("timeout", FAKE_TIMEOUT),
    ] {
        let p = bin.join(name);
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let list = dir.path().join("mirrors");
    if let Some(urls) = mirrors {
        let text: String = urls
            .iter()
            .enumerate()
            .map(|(i, u)| format!("{u}\tpriority:{}\n", i + 1))
            .collect();
        fs::write(&list, text).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(script())
        .args(["--no-install-recommends", "libfoo"])
        .env("PATH", path)
        .env("FAKE_DIR", dir.path())
        .env("FAKE_OUTCOMES", outcomes)
        .env("APT_PREFETCH_MIRRORS", &list)
        .output()
        .expect("bash runs");
    Run { dir, out }
}

/// Scenario: the image lists a regional EC2 mirror first and it times out;
/// the second attempt fetches from archive.ubuntu.com and the script succeeds.
#[test]
fn apt_prefetch_001_a_stalled_first_mirror_that_is_not_azure_is_dropped() {
    let r = run(Some(&[EC2, ARCHIVE, SECURITY]), "124 0");
    assert!(r.out.status.success(), "{}", r.stdout());
    assert_eq!(r.used(), [EC2, ARCHIVE]);
    assert_eq!(r.mirrors_left(), [ARCHIVE, SECURITY]);
    assert!(
        r.stdout().contains(&format!("dropped {EC2}")),
        "{}",
        r.stdout()
    );
}

/// Scenario: the older image's Azure mirror first, as the script was written
/// against; it is still dropped after a stalled attempt.
#[test]
fn apt_prefetch_002_the_azure_mirror_is_still_dropped() {
    let r = run(Some(&[AZURE, ARCHIVE, SECURITY]), "124 0");
    assert!(r.out.status.success(), "{}", r.stdout());
    assert_eq!(r.used(), [AZURE, ARCHIVE]);
}

/// Scenario: every attempt times out. The script moves down the list once and
/// then stays on archive.ubuntu.com rather than leaving security.ubuntu.com,
/// which serves only the security pocket, alone in the list; it fails.
#[test]
fn apt_prefetch_003_security_ubuntu_com_is_never_left_alone() {
    let r = run(Some(&[EC2, ARCHIVE, SECURITY]), "124 124 124");
    assert_eq!(r.out.status.code(), Some(1), "{}", r.stdout());
    assert_eq!(r.used(), [EC2, ARCHIVE, ARCHIVE]);
    assert_eq!(r.mirrors_left(), [ARCHIVE, SECURITY]);

    let r = run(Some(&[ARCHIVE, SECURITY]), "124 124 124");
    assert_eq!(r.out.status.code(), Some(1), "{}", r.stdout());
    assert_eq!(r.used(), [ARCHIVE, ARCHIVE, ARCHIVE]);
}

/// Scenario: a runner with one mirror, or with no mirror list at all; the
/// script retries in place without dropping anything and without dying on the
/// empty count.
#[test]
fn apt_prefetch_004_one_mirror_or_none_retries_in_place() {
    let r = run(Some(&[ARCHIVE]), "124 0");
    assert!(r.out.status.success(), "{}", r.stdout());
    assert_eq!(r.used(), [ARCHIVE, ARCHIVE]);
    assert_eq!(r.mirrors_left(), [ARCHIVE]);

    let r = run(None, "124 0");
    assert!(r.out.status.success(), "{}", r.stdout());
    assert_eq!(r.used(), ["-", "-"]);
}

/// Scenario: the first attempt finishes; nothing is dropped.
#[test]
fn apt_prefetch_005_a_finished_first_attempt_changes_nothing() {
    let r = run(Some(&[EC2, ARCHIVE, SECURITY]), "0");
    assert!(r.out.status.success(), "{}", r.stdout());
    assert_eq!(r.used(), [EC2]);
    assert_eq!(r.mirrors_left(), [EC2, ARCHIVE, SECURITY]);
}
