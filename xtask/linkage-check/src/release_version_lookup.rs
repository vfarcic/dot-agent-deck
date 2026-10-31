//! Issue #2052 follow-up: `scripts/latest-release-version.sh` is the one copy
//! of the lookup that gives a tagless CI or sandbox build its `DAD_VERSION`
//! (issue #326). It replaced three inline copies, in `.github/workflows/ci.yml`,
//! `.rwx/ci.yml` and `.rwx/sandbox.yml`, which nothing kept in step: a fix to
//! one (the version pattern, the pre-release rule) would have left the others
//! choosing a different version for the same commit.
//!
//! These tests hold both halves: the script's choice, driven under `bash`
//! against an offline `git` stand-in, so no test reads the network or runs
//! git; and each caller running the script rather than carrying its own copy,
//! read from the files themselves.
//!
//! Unix-only, like `release_channel_vars`: the script needs a POSIX shell, and
//! its callers run it on Linux.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use yaml_rust2::{Yaml, YamlLoader};

const SCRIPT: &str = "scripts/latest-release-version.sh";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

/// A `git` stand-in that records its arguments and answers `ls-remote` with
/// `$LOOKUP_STUB_TAGS`, or fails when `$LOOKUP_STUB_FAIL` is set.
const GIT_STUB: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "$LOOKUP_STUB_LOG"
if [ -n "${LOOKUP_STUB_FAIL:-}" ]; then
  echo "fatal: could not read from remote repository" >&2
  exit 128
fi
printf '%s' "$LOOKUP_STUB_TAGS"
"#;

struct Run {
    out: Output,
    calls: String,
}

/// Runs the script with `remote` against the stand-in, which lists `tags`
/// (one tag name per entry, as `git ls-remote --refs` prints them) or fails.
fn run(remote: &str, tags: &[&str], fail: bool) -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let git = bin.join("git");
    fs::write(&git, GIT_STUB).unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.path().join("calls.log");
    fs::write(&log, "").unwrap();
    let listing: String = tags
        .iter()
        .map(|t| format!("0123456789abcdef0123456789abcdef01234567\trefs/tags/{t}\n"))
        .collect();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("bash");
    cmd.arg(repo_root().join(SCRIPT))
        .arg(remote)
        .current_dir(dir.path())
        .env("PATH", path)
        .env("LOOKUP_STUB_LOG", &log)
        .env("LOOKUP_STUB_TAGS", listing);
    if fail {
        cmd.env("LOOKUP_STUB_FAIL", "1");
    } else {
        cmd.env_remove("LOOKUP_STUB_FAIL");
    }
    let out = cmd.output().expect("run bash");
    let calls = fs::read_to_string(&log).unwrap();
    Run { out, calls }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn release_version_lookup_001_prints_the_newest_release_by_version_order() {
    let r = run(
        "origin",
        &[
            "v0.9.0",
            "v0.10.0",
            "v0.49.10",
            "v0.49.4",
            // Neither a pre-release nor anything short of X.Y.Z is a release a
            // developer's `main` reports, however high it sorts.
            "v1.0.0-alpha.0",
            "v2.0",
            "v..",
            "vnext",
        ],
        false,
    );
    assert!(
        r.out.status.success(),
        "the lookup failed: {}",
        stderr(&r.out)
    );
    assert_eq!(
        stdout(&r.out),
        "0.49.10\n",
        "the newest X.Y.Z by version order, without its `v`, and only that"
    );
    assert_eq!(
        r.calls.trim(),
        "ls-remote --tags --refs origin v*",
        "the tags are read from the remote it was given, and only the `v*` ones"
    );
}

#[test]
fn release_version_lookup_002_a_remote_with_no_release_tag_is_an_error() {
    let r = run(
        "https://example.invalid/repo.git",
        &["v1.0.0-rc.1", "nightly"],
        false,
    );
    assert!(
        !r.out.status.success(),
        "no release tag must fail the caller rather than hand it an empty version"
    );
    assert_eq!(
        stdout(&r.out),
        "",
        "nothing a caller could export as a version"
    );
    assert!(
        stderr(&r.out).contains("https://example.invalid/repo.git"),
        "the error names the remote: {}",
        stderr(&r.out)
    );
}

#[test]
fn release_version_lookup_003_an_unreadable_remote_is_an_error() {
    let r = run("origin", &["v0.49.4"], true);
    assert!(
        !r.out.status.success(),
        "a failed `git ls-remote` must fail the caller"
    );
    assert_eq!(
        stdout(&r.out),
        "",
        "nothing a caller could export as a version"
    );
}

/// Whether a `run` body invokes the script on a line that is not a shell
/// comment. A comment naming the script is not a call to it, and both RWX
/// callers carry one (PR #2056 review).
fn runs_script(run: &str) -> bool {
    run.lines()
        .map(str::trim_start)
        .any(|l| !l.starts_with('#') && l.contains(SCRIPT))
}

fn load_yaml(file: &str) -> Yaml {
    let text =
        fs::read_to_string(repo_root().join(file)).unwrap_or_else(|e| panic!("read {file}: {e}"));
    YamlLoader::load_from_str(&text)
        .unwrap_or_else(|e| panic!("{file} is not valid YAML: {e}"))
        .remove(0)
}

/// The `run` body of the item in `items` whose `field` is `value`: an RWX
/// task by its `key`, a GitHub Actions step by its `id`.
fn run_of<'a>(items: &'a Yaml, field: &str, value: &str, file: &str) -> &'a str {
    items
        .as_vec()
        .unwrap_or_else(|| panic!("{file}: expected a list of tasks or steps"))
        .iter()
        .find(|i| i[field].as_str() == Some(value))
        .unwrap_or_else(|| panic!("{file} has no {field} `{value}`; update this test"))["run"]
        .as_str()
        .unwrap_or_else(|| panic!("{file}'s {field} `{value}` has no `run` body"))
}

#[test]
fn release_version_lookup_004_a_comment_naming_the_script_is_not_a_call() {
    assert!(runs_script(&format!("set -e\nv=$({SCRIPT} origin)\n")));
    assert!(runs_script(&format!(
        "  if x; then\n    v=$({SCRIPT} origin)\n  fi"
    )));
    assert!(
        !runs_script(&format!("# {SCRIPT} reads the tag\nv=0.1.0\n")),
        "a body whose only mention of the script is a comment does not run it"
    );
    assert!(!runs_script("v=$(git describe --tags)"));
}

#[test]
fn release_version_lookup_005_every_caller_runs_the_script_and_keeps_no_copy() {
    let ci = load_yaml(".github/workflows/ci.yml");
    let rwx_ci = load_yaml(".rwx/ci.yml");
    let sandbox = load_yaml(".rwx/sandbox.yml");
    let callers = [
        (
            ".github/workflows/ci.yml",
            run_of(&ci["jobs"]["changes"]["steps"], "id", "version", "ci.yml"),
        ),
        (
            ".rwx/ci.yml",
            run_of(&rwx_ci["tasks"], "key", "backend", ".rwx/ci.yml"),
        ),
        (
            ".rwx/sandbox.yml",
            run_of(&sandbox["tasks"], "key", "version", ".rwx/sandbox.yml"),
        ),
    ];
    for (file, run) in callers {
        assert!(
            runs_script(run),
            "{file}'s version lookup no longer runs {SCRIPT}, so its builds may get no version \
             or a different one"
        );
    }
    for file in [
        ".github/workflows/ci.yml",
        ".rwx/ci.yml",
        ".rwx/sandbox.yml",
    ] {
        let text = fs::read_to_string(repo_root().join(file)).unwrap();
        assert!(
            !text.contains("ls-remote --tags"),
            "{file} lists tags itself: run {SCRIPT} instead of keeping a second copy of the lookup"
        );
    }
}

/// The `changes` job checks out only the files it names, so a script it runs
/// that its sparse checkout leaves out is a step that fails on every run.
#[test]
fn release_version_lookup_006_the_changes_job_checks_the_script_out() {
    let ci = load_yaml(".github/workflows/ci.yml");
    let steps = ci["jobs"]["changes"]["steps"]
        .as_vec()
        .expect("ci.yml has a `changes` job with steps");
    assert!(
        steps
            .iter()
            .any(|s| s["run"].as_str().is_some_and(runs_script)),
        "the `changes` job no longer runs {SCRIPT}; update this test"
    );
    let sparse: Vec<&str> = steps
        .iter()
        .filter(|s| {
            s["uses"]
                .as_str()
                .is_some_and(|u| u.starts_with("actions/checkout@"))
        })
        .filter_map(|s| match &s["with"]["sparse-checkout"] {
            Yaml::String(v) => Some(v.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !sparse.is_empty(),
        "the `changes` job's checkout is no longer sparse; update this test"
    );
    assert!(
        sparse.iter().any(|v| v.lines().any(|l| l.trim() == SCRIPT)),
        "the `changes` job runs {SCRIPT} but its sparse checkout does not include it: {sparse:?}"
    );
}
