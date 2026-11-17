//! `scripts/ci-unacquired-runners.sh` decides whether a CI attempt failed only
//! because GitHub never gave a job a runner. On that answer
//! `rerun-unacquired.yml` reruns the run and `notify-main-red` holds its
//! alert, so a wrong `UNACQUIRED_ONLY` would hide a real red. These tests run
//! the script under `bash` against a fake `gh` that serves canned API pages
//! from a temporary directory: no network, no token.
//!
//! The last tests read the two workflows and the script, and fail when the
//! pieces that must agree drift apart: the one-rerun bound on both sides, the
//! held-alert annotation title, and the aggregate jobs naming real jobs.
//!
//! Unix-only, like `release_channel_vars`: the script is bash, and its callers
//! are Ubuntu jobs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join("scripts/ci-unacquired-runners.sh")
}

/// A stand-in for `gh api`: it serves `jobs.json` for the attempt's jobs URL
/// and `ann-<id>.json` for a check run's annotations, applying `--jq` with the
/// real `jq` to every JSON document in the file (several documents stand for
/// several pages of `--paginate`). A missing file is an API error. Every URL
/// it is asked for is appended to `calls`.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
url=""; expr="."
while [ $# -gt 0 ]; do
  case "$1" in
    api|--paginate) ;;
    --jq) shift; expr="$1" ;;
    *) url="$1" ;;
  esac
  shift
done
echo "$url" >> "$FAKE_GH_DIR/calls"
case "$url" in
  */attempts/*/jobs\?per_page=100) f="$FAKE_GH_DIR/jobs.json" ;;
  */check-runs/*/annotations) id="${url%/annotations}"; f="$FAKE_GH_DIR/ann-${id##*/}.json" ;;
  *) echo "fake gh: unexpected URL $url" >&2; exit 1 ;;
esac
[ -f "$f" ] || { echo "fake gh: HTTP 502 for $url" >&2; exit 1; }
jq -r "$expr" "$f"
"#;

const NOT_ACQUIRED: &str =
    "The job was not acquired by Runner of type hosted even after multiple attempts";

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        let gh = dir.path().join("bin/gh");
        fs::write(&gh, FAKE_GH).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        Fake { dir }
    }

    /// One page per element of `pages`, each a list of jobs.
    fn jobs(&self, pages: &[Vec<serde_json::Value>]) -> &Self {
        let text: String = pages
            .iter()
            .map(|jobs| {
                serde_json::json!({ "total_count": jobs.len(), "jobs": jobs }).to_string() + "\n"
            })
            .collect();
        fs::write(self.dir.path().join("jobs.json"), text).unwrap();
        self
    }

    fn annotations(&self, id: u64, notes: &[(&str, &str)]) -> &Self {
        let list: Vec<_> = notes
            .iter()
            .map(|(title, message)| serde_json::json!({ "title": title, "message": message }))
            .collect();
        fs::write(
            self.dir.path().join(format!("ann-{id}.json")),
            serde_json::Value::Array(list).to_string(),
        )
        .unwrap();
        self
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("bash")
            .arg(script())
            .args(args)
            .env("PATH", path)
            .env("FAKE_GH_DIR", self.dir.path())
            .env("GITHUB_REPOSITORY", "owner/repo")
            .output()
            .expect("bash runs")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }
}

fn job(
    id: u64,
    name: &str,
    status: &str,
    conclusion: Option<&str>,
    runner: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "runner_name": runner,
        "check_run_url": format!("https://api.github.com/repos/owner/repo/check-runs/{id}"),
    })
}

fn passed(id: u64, name: &str) -> serde_json::Value {
    job(id, name, "completed", Some("success"), "GitHub Actions 1")
}

fn unacquired(id: u64, name: &str) -> serde_json::Value {
    job(id, name, "completed", Some("cancelled"), "")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `jq` is what the script parses with; without it the script cannot run at
/// all, on a runner or here, so its absence is a failure, not a skip.
fn tools_present() {
    for tool in ["bash", "jq"] {
        let ok = Command::new(tool)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            ok,
            "{tool} is needed to test scripts/ci-unacquired-runners.sh"
        );
    }
}

/// The 2026-11-16 shape: macOS jobs that never got a runner, their aggregate
/// red, every other job passed or skipped, and `notify-main-red` still running.
#[test]
fn ci_unacquired_001_jobs_that_never_got_a_runner_are_the_only_failure() {
    tools_present();
    let fake = Fake::new();
    fake.jobs(&[vec![
        passed(1, "changes"),
        unacquired(2, "build-macos-clippy"),
        unacquired(3, "build-macos-tests (2)"),
        passed(4, "build-macos-tests (1)"),
        job(
            5,
            "build-macos",
            "completed",
            Some("failure"),
            "GitHub Actions 2",
        ),
        job(6, "build-windows", "completed", Some("skipped"), ""),
        job(
            7,
            "notify-main-red",
            "in_progress",
            None,
            "GitHub Actions 3",
        ),
    ]])
    .annotations(2, &[("", NOT_ACQUIRED)])
    .annotations(
        3,
        &[("", NOT_ACQUIRED), ("", "Due to capacity constraints, …")],
    );
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(0), "stdout: {}", stdout(&out));
    assert_eq!(
        stdout(&out),
        "UNACQUIRED_ONLY build-macos-clippy,build-macos-tests (2)"
    );
    assert!(
        fake.calls()
            .contains("repos/owner/repo/actions/runs/123/attempts/1/jobs?per_page=100"),
        "asked for the wrong attempt: {}",
        fake.calls()
    );
}

#[test]
fn ci_unacquired_002_a_real_failure_beside_them_is_other() {
    tools_present();
    let fake = Fake::new();
    fake.jobs(&[vec![
        unacquired(2, "build-macos-clippy"),
        job(
            3,
            "e2e (2)",
            "completed",
            Some("failure"),
            "GitHub Actions 1",
        ),
        job(
            4,
            "e2e-deterministic",
            "completed",
            Some("failure"),
            "GitHub Actions 2",
        ),
    ]])
    .annotations(2, &[("", NOT_ACQUIRED)]);
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "OTHER e2e (2)=failure");
}

#[test]
fn ci_unacquired_003_a_job_cancelled_after_a_runner_took_it_is_other() {
    tools_present();
    let fake = Fake::new();
    fake.jobs(&[vec![job(
        2,
        "build-macos-tests (1)",
        "completed",
        Some("cancelled"),
        "GitHub Actions 9",
    )]])
    .annotations(2, &[("", NOT_ACQUIRED)]);
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "OTHER build-macos-tests (1)=cancelled");
}

/// A queued job cancelled for another reason, or a GitHub rewording of the
/// annotation, gives the old behaviour rather than a rerun.
#[test]
fn ci_unacquired_004_no_runner_without_the_annotation_is_other() {
    tools_present();
    let fake = Fake::new();
    fake.jobs(&[vec![unacquired(2, "build-macos-clippy")]])
        .annotations(2, &[("", "The operation was canceled.")]);
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "OTHER build-macos-clippy=cancelled");
}

#[test]
fn ci_unacquired_005_an_unreadable_run_or_annotation_is_exit_2() {
    tools_present();
    let fake = Fake::new();
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(2), "no jobs page: {}", stdout(&out));

    fake.jobs(&[vec![unacquired(2, "build-macos-clippy")]]);
    let out = fake.run(&["123", "1"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "no annotations: {}",
        stdout(&out)
    );
    assert!(stdout(&out).starts_with("OTHER "), "{}", stdout(&out));

    let out = fake.run(&["12x", "1"]);
    assert_eq!(out.status.code(), Some(2), "a run id that is not a number");
}

/// A failure on a later page of `--paginate` still counts.
#[test]
fn ci_unacquired_006_every_page_of_jobs_is_read() {
    tools_present();
    let fake = Fake::new();
    fake.jobs(&[
        vec![unacquired(2, "build-macos-clippy"), passed(3, "fmt")],
        vec![job(
            4,
            "security",
            "completed",
            Some("failure"),
            "GitHub Actions 1",
        )],
    ])
    .annotations(2, &[("", NOT_ACQUIRED)]);
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "OTHER security=failure");
}

/// Only an aggregate red, or nothing red at all, is not a capacity failure.
/// And `build-windows` is not an aggregate: it builds and tests when enabled.
#[test]
fn ci_unacquired_007_aggregates_alone_and_build_windows_are_other() {
    tools_present();
    let fake = Fake::new();
    fake.jobs(&[vec![job(
        5,
        "build-macos",
        "completed",
        Some("failure"),
        "GitHub Actions 2",
    )]]);
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(1));

    fake.jobs(&[vec![passed(1, "build")]]);
    assert_eq!(fake.run(&["123", "1"]).status.code(), Some(1));

    fake.jobs(&[vec![
        unacquired(2, "build-macos-clippy"),
        job(
            6,
            "build-windows",
            "completed",
            Some("failure"),
            "GitHub Actions 4",
        ),
    ]])
    .annotations(2, &[("", NOT_ACQUIRED)]);
    let out = fake.run(&["123", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "OTHER build-windows=failure");
}

#[test]
fn ci_unacquired_008_alert_held_reads_notify_main_reds_annotation() {
    tools_present();
    let fake = Fake::new();
    fake.jobs(&[vec![job(
        7,
        "notify-main-red",
        "completed",
        Some("success"),
        "GitHub Actions 3",
    )]])
    .annotations(
        7,
        &[("main-red alert held", "UNACQUIRED_ONLY build-macos-clippy")],
    );
    let out = fake.run(&["--alert-held", "123", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    fake.annotations(7, &[("main is red", "abc12345 — failed: build-macos")]);
    assert_eq!(
        fake.run(&["--alert-held", "123", "1"]).status.code(),
        Some(1),
        "it sent the alert"
    );

    fake.jobs(&[vec![job(
        7,
        "notify-main-red",
        "completed",
        Some("skipped"),
        "",
    )]]);
    assert_eq!(
        fake.run(&["--alert-held", "123", "1"]).status.code(),
        Some(1),
        "it did not run"
    );

    fake.jobs(&[vec![job(
        8,
        "notify-main-red",
        "completed",
        Some("success"),
        "GitHub Actions 3",
    )]]);
    assert_eq!(
        fake.run(&["--alert-held", "123", "1"]).status.code(),
        Some(2),
        "unreadable annotations are not 'not held'"
    );
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The bound, the marker and the caller have to match across three files.
#[test]
fn ci_unacquired_009_the_workflows_and_the_script_agree() {
    let rerun = read(".github/workflows/rerun-unacquired.yml");
    let ci = read(".github/workflows/ci.yml");
    let sh = read("scripts/ci-unacquired-runners.sh");

    assert!(
        rerun.contains("github.event.workflow_run.run_attempt == 1"),
        "rerun-unacquired.yml lost its one-rerun bound"
    );
    assert!(
        ci.contains(r#"[ "${GITHUB_RUN_ATTEMPT:-1}" -eq 1 ]"#),
        "notify-main-red no longer holds on attempt 1 only"
    );
    assert!(
        ci.contains("scripts/ci-unacquired-runners.sh \"$GITHUB_RUN_ID\""),
        "notify-main-red no longer asks the script"
    );
    assert!(
        rerun.contains("scripts/ci-unacquired-runners.sh --alert-held"),
        "rerun-unacquired.yml no longer sends a held alert"
    );

    let marker = sh
        .lines()
        .find_map(|l| {
            l.strip_prefix("HELD_MARKER='")
                .and_then(|r| r.strip_suffix('\''))
        })
        .expect("the script defines HELD_MARKER");
    assert!(
        ci.contains(&format!("::notice title={marker}::")),
        "notify-main-red's annotation title is not the script's HELD_MARKER ({marker})"
    );

    let aggregates = sh
        .lines()
        .find_map(|l| {
            l.strip_prefix("AGGREGATES='")
                .and_then(|r| r.strip_suffix('\''))
        })
        .expect("the script defines AGGREGATES");
    let aggregates: Vec<String> =
        serde_json::from_str(aggregates).expect("AGGREGATES is a JSON list");
    for name in aggregates {
        let block = job_block(&ci, &name)
            .unwrap_or_else(|| panic!("AGGREGATES names `{name}`, which is not a job in ci.yml"));
        let code: Vec<&str> = block
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        let runs = code
            .iter()
            .filter(|l| l.trim_start().starts_with("run:") || l.trim_start().starts_with("- run:"))
            .count();
        assert!(
            runs == 1
                && code
                    .iter()
                    .any(|l| l.contains("NEEDS_JSON: ${{ toJSON(needs) }}"))
                && !code
                    .iter()
                    .any(|l| l.contains("uses:") || l.contains("cargo")),
            "`{name}` is in AGGREGATES but is no longer a job whose one step only reads `needs`"
        );
    }
}

/// The lines of one top-level job in ci.yml, up to the next job.
fn job_block(ci: &str, name: &str) -> Option<String> {
    let header = format!("  {name}:");
    let mut lines = ci.lines().skip_while(|l| *l != header);
    lines.next()?;
    let is_job = |l: &&str| {
        l.len() > 2 && l.starts_with("  ") && !l[2..].starts_with([' ', '#']) && l.ends_with(':')
    };
    Some(
        lines
            .take_while(|l| !is_job(l))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}
