//! Issue #2072 — the RWX sandbox's `cargo` shim, `scripts/rwx-sandbox-bin/cargo`.
//!
//! Every `rwx sandbox exec` rewrites `.git/HEAD`, the branch ref, the reflog
//! and the index while syncing, so `build.rs`'s mtime watches on them rebuilt
//! the root crate on every command. The shim hands the real cargo a
//! `DAD_BUILD_ID` composed from the checkout's git content, which makes
//! `build.rs` skip git (`build_needs_git`) and lets
//! `cargo:rerun-if-env-changed=DAD_BUILD_ID` decide instead.
//!
//! What has to hold is that the value is the one `build.rs` would have
//! composed itself, so these tests run the script under `bash` against a `git`
//! stand-in (no test runs git) and a stand-in "real" cargo that prints what it
//! received, and compare the result with `resolve_build_id` from the shared
//! `build_version_resolve.rs`. The stand-in pins the git commands the shim
//! runs, and `the_shim_asks_git_what_build_rs_asks` holds them to `build.rs`'s.
//!
//! Unix-only: the script needs bash, and only the Linux sandbox runs it.
#![cfg(unix)]

#[path = "../build_version_resolve.rs"]
#[allow(dead_code)]
mod build_version_resolve;

use build_version_resolve::{
    VersionSource, build_needs_git, normalize_build_id, normalize_version, resolve_build_id,
};

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SHIM: &str = "scripts/rwx-sandbox-bin/cargo";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A `git` stand-in: records each call's arguments, answers the three
/// questions the shim asks from `$STUB_*`, and fails a call whose arguments
/// it does not know, so a changed command shows up as a missing build id.
const GIT_STUB: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_LOG"
case "$*" in
  "rev-parse --show-toplevel") printf '%s\n' "$STUB_TOP" ;;
  "rev-parse --short HEAD") printf '%s\n' "$STUB_SHA" ;;
  "--no-optional-locks status --porcelain")
    [ -z "${STUB_STATUS_FAIL:-}" ] || exit 128
    printf '%s' "$STUB_STATUS" ;;
  *) echo "git stub: unexpected call: $*" >&2; exit 2 ;;
esac
"#;

/// The "real" cargo the shim must find after itself on PATH: prints the
/// `DAD_BUILD_ID` it was given (or `<unset>`) and each argument on its own
/// line, and exits with `$STUB_CARGO_EXIT`.
const CARGO_STUB: &str = r#"#!/usr/bin/env bash
printf 'DAD_BUILD_ID=%s\n' "${DAD_BUILD_ID-<unset>}"
for a in "$@"; do printf 'arg=%s\n' "$a"; done
exit "${STUB_CARGO_EXIT:-0}"
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    stubs: PathBuf,
    workspace: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().unwrap();
        let stubs = root.join("stubs");
        let workspace = root.join("workspace");
        fs::create_dir_all(&stubs).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        for (name, body) in [("git", GIT_STUB), ("cargo", CARGO_STUB)] {
            let path = stubs.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = root.join("git-calls.log");
        fs::write(&log, "").unwrap();
        Fixture {
            _dir: dir,
            stubs,
            workspace,
            log,
        }
    }

    /// Runs the shim from the workspace with the stand-ins after it on PATH.
    /// `env` sets variables; a `None` value removes one. By default the
    /// checkout is the sandbox's, at sha `abc1234`, clean, with `DAD_VERSION`
    /// 0.51.0 and no `DAD_BUILD_ID`.
    fn run(&self, args: &[&str], env: &[(&str, Option<&str>)]) -> Output {
        let shim = repo_root().join(SHIM);
        let path = format!(
            "{}:{}:{}",
            shim.parent().unwrap().display(),
            self.stubs.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("bash");
        cmd.arg(&shim)
            .args(args)
            .current_dir(&self.workspace)
            .env("PATH", path)
            .env("STUB_LOG", &self.log)
            .env("STUB_TOP", &self.workspace)
            .env("STUB_SHA", "abc1234")
            .env("STUB_STATUS", "")
            .env("DAD_VERSION", "0.51.0")
            .env("RWX_WORKSPACE_DIR", &self.workspace)
            .env_remove("DAD_BUILD_ID")
            .env_remove("STUB_STATUS_FAIL")
            .env_remove("STUB_CARGO_EXIT");
        for (k, v) in env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        cmd.output().expect("run bash")
    }

    fn git_calls(&self) -> String {
        fs::read_to_string(&self.log).unwrap()
    }
}

/// The `DAD_BUILD_ID` the stand-in cargo received, `None` when unset.
fn received_build_id(out: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("DAD_BUILD_ID="))
        .unwrap_or_else(|| {
            panic!(
                "the real cargo was not reached; stdout: {stdout}; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    (line != "<unset>").then(|| line.to_string())
}

/// What `build.rs` composes for the same checkout when nothing is injected.
fn build_rs_would_compose(dad_version: &str, sha: &str, dirty: bool) -> String {
    let version = normalize_version(dad_version).expect("a valid version");
    resolve_build_id(None, &version, Some(sha), dirty)
}

/// What `build.rs` does with the shim's value: it must accept it verbatim and
/// then skip git entirely.
fn assert_build_rs_takes_it(id: &str) {
    assert_eq!(normalize_build_id(id).as_deref(), Some(id));
    assert!(
        !build_needs_git(VersionSource::Injected, Some(id)),
        "with DAD_VERSION and this id injected, build.rs must run no git and watch no git file"
    );
}

#[test]
fn sandbox_cargo_shim_001_a_clean_checkout_gets_the_id_build_rs_would_compose() {
    let f = Fixture::new();
    let out = f.run(&["build", "--workspace"], &[]);
    assert!(out.status.success());
    let id = received_build_id(&out).expect("a build id for the sandbox checkout");
    assert_eq!(id, build_rs_would_compose("0.51.0", "abc1234", false));
    assert_eq!(id, "0.51.0-gabc1234");
    assert_build_rs_takes_it(&id);
}

#[test]
fn sandbox_cargo_shim_002_a_dirty_checkout_is_marked_dirty() {
    let f = Fixture::new();
    let out = f.run(&["build"], &[("STUB_STATUS", Some(" M src/main.rs\n"))]);
    let id = received_build_id(&out).expect("a build id");
    assert_eq!(id, build_rs_would_compose("0.51.0", "abc1234", true));
    assert_eq!(id, "0.51.0-gabc1234-dirty");
    assert_build_rs_takes_it(&id);
}

#[test]
fn sandbox_cargo_shim_003_a_v_prefixed_version_is_stripped_as_build_rs_strips_it() {
    let f = Fixture::new();
    let out = f.run(&["build"], &[("DAD_VERSION", Some("v0.51.0"))]);
    let id = received_build_id(&out).expect("a build id");
    assert_eq!(id, build_rs_would_compose("v0.51.0", "abc1234", false));
}

#[test]
fn sandbox_cargo_shim_004_an_injected_build_id_is_left_alone_and_git_is_not_asked() {
    let f = Fixture::new();
    let out = f.run(&["build"], &[("DAD_BUILD_ID", Some("42.7.13-ginjected0"))]);
    assert_eq!(
        received_build_id(&out).as_deref(),
        Some("42.7.13-ginjected0")
    );
    assert_eq!(f.git_calls(), "", "an injected id needs no git");
}

#[test]
fn sandbox_cargo_shim_005_no_id_unless_build_rs_would_take_the_version_from_dad_version() {
    // Without a plain X.Y.Z `DAD_VERSION`, build.rs would take the version
    // from git or the placeholder, so an id composed here would not match.
    for version in [
        None,
        Some(""),
        Some("  "),
        Some("0.51.0-rc.1"),
        Some("next"),
    ] {
        let f = Fixture::new();
        let out = f.run(&["build"], &[("DAD_VERSION", version)]);
        assert_eq!(
            received_build_id(&out),
            None,
            "DAD_VERSION={version:?} must leave the build id to build.rs"
        );
    }
}

#[test]
fn sandbox_cargo_shim_006_no_id_outside_the_sandbox_checkout() {
    let f = Fixture::new();
    let elsewhere = f.workspace.parent().unwrap().join("stubs");
    let out = f.run(
        &["build"],
        &[("STUB_TOP", Some(elsewhere.to_str().unwrap()))],
    );
    assert_eq!(
        received_build_id(&out),
        None,
        "another checkout's build id is not this one's"
    );

    let out = f.run(&["build"], &[("RWX_WORKSPACE_DIR", None)]);
    assert_eq!(
        received_build_id(&out),
        None,
        "outside a sandbox there is no checkout to vouch for"
    );
}

#[test]
fn sandbox_cargo_shim_007_a_failing_git_leaves_the_id_to_build_rs() {
    let f = Fixture::new();
    let out = f.run(&["build"], &[("STUB_STATUS_FAIL", Some("1"))]);
    assert_eq!(received_build_id(&out), None);

    let out = f.run(&["build"], &[("STUB_SHA", Some(""))]);
    assert_eq!(received_build_id(&out), None);
}

#[test]
fn sandbox_cargo_shim_008_arguments_and_exit_status_pass_through() {
    let f = Fixture::new();
    let out = f.run(
        &["nextest", "run", "a filter with spaces", "--", "--exact"],
        &[("STUB_CARGO_EXIT", Some("3"))],
    );
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let args: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("arg="))
        .collect();
    assert_eq!(
        args,
        ["nextest", "run", "a filter with spaces", "--", "--exact"]
    );
}

/// The shim's value equals `build.rs`'s only while both ask git the same two
/// questions, so a change to either command in `build.rs` fails here.
#[test]
fn sandbox_cargo_shim_009_the_shim_asks_git_what_build_rs_asks() {
    let build_rs = fs::read_to_string(repo_root().join("build.rs")).unwrap();
    for args in [
        r#"["rev-parse", "--short", "HEAD"]"#,
        r#"["--no-optional-locks", "status", "--porcelain"]"#,
    ] {
        assert!(
            build_rs.contains(args),
            "build.rs no longer runs git {args}; update {SHIM} to match"
        );
    }
    let f = Fixture::new();
    f.run(&["build"], &[]);
    assert_eq!(
        f.git_calls(),
        "rev-parse --show-toplevel\nrev-parse --short HEAD\n--no-optional-locks status --porcelain\n"
    );
}

/// The sandbox puts the shim's directory on PATH; without it the shim is dead
/// code and every exec rebuilds the root crate again.
#[test]
fn sandbox_cargo_shim_010_the_sandbox_puts_the_shim_on_path() {
    let yml = fs::read_to_string(repo_root().join(".rwx/sandbox.yml")).unwrap();
    assert!(
        yml.contains(
            r#"printf '%s\n' "$RWX_WORKSPACE_DIR/scripts/rwx-sandbox-bin" > "$RWX_ENV/PATH""#
        ),
        ".rwx/sandbox.yml must export scripts/rwx-sandbox-bin on PATH"
    );
    let uses = yml
        .lines()
        .skip_while(|l| l.trim() != "- key: sandbox")
        .find_map(|l| l.trim().strip_prefix("use: "))
        .expect("the sandbox task's `use` line");
    assert!(
        uses.contains("cargo-build-id"),
        "the sandbox task must use the cargo-build-id task: {uses}"
    );
    let mode = fs::metadata(repo_root().join(SHIM))
        .unwrap()
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "{SHIM} must be executable to be found on PATH"
    );
    assert!(Path::new(SHIM).file_name() == Some("cargo".as_ref()));
}
