//! Issue #1256: knowing when the automated review apps have finished.
//!
//! The skills that drive a pull request (`pr-create`, `issue-queue`,
//! `prd-queue`, `land-prs`, `code-cleanup`) and the orchestrator in
//! `.dot-agent-deck.toml` used to describe the wait per app, and it had gone
//! wrong both ways: keyed on a check-run, which Qodo never creates, so the wait
//! ran out its whole budget; or vague, so an agent read the threads before Qodo
//! had finished. They now call `.claude/skills/pr-create/wait-reviews.sh`, which
//! reads the apps and their completion signals from `reviewers.json` beside it,
//! so replacing a reviewer is one entry rather than an edit to every skill.
//!
//! What the script decides is a runtime property — which comment counts, for
//! which commit, and when the bound runs out — so it is driven here against an
//! offline `gh` stand-in, the shape `prd_skills.rs` uses, and no test touches
//! the network. The real `reviewers.json` is used wherever the test is about
//! Qodo or Greptile, so an edit to it that breaks either signal goes red here.
//!
//! Unix-only: the script needs `bash` and `jq`; where either cannot be spawned
//! the tests print `SKIP:` and return.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const HEAD: &str = "1111111111111111111111111111111111111111";
const OLD: &str = "2222222222222222222222222222222222222222";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join(".claude/skills/pr-create/wait-reviews.sh")
}

fn tools_present() -> bool {
    let present = |tool: &str| {
        Command::new(tool)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if present("bash") && present("jq") {
        return true;
    }
    eprintln!("SKIP: bash or jq cannot be spawned; the wait-reviews.sh tests need both");
    false
}

/// An offline `gh`. Each surface the script reads answers from
/// `$REVIEW_STUB/<surface>.<call>.json` when that file exists, else from
/// `<surface>.json`, and fails when neither does — so a test can change the
/// answer between polls, or make a read fail.
const GH_STUB: &str = r#"#!/usr/bin/env bash
answer() {
  local n
  n=$(( $(cat "$REVIEW_STUB/$1.count" 2>/dev/null || echo 0) + 1 ))
  echo "$n" >"$REVIEW_STUB/$1.count"
  if [ -f "$REVIEW_STUB/$1.$n.json" ]; then cat "$REVIEW_STUB/$1.$n.json"
  elif [ -f "$REVIEW_STUB/$1.json" ]; then cat "$REVIEW_STUB/$1.json"
  else echo "HTTP 502" >&2; exit 1
  fi
}
case "$*" in
  "pr view "*) answer pr ;;
  "api repos/{owner}/{repo}/issues/"*"/comments --paginate") answer comments ;;
  "api repos/{owner}/{repo}/commits/"*"/check-runs?per_page=100 --paginate") answer checks ;;
  "api repos/{owner}/{repo}/pulls/"*"/reviews --paginate") answer reviews ;;
  *) echo "gh stand-in: unhandled invocation: $*" >&2; exit 1 ;;
esac
"#;

struct Sandbox {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    stub: PathBuf,
    registry: Option<PathBuf>,
}

impl Sandbox {
    /// A pull request whose head is `HEAD`, opened `age_minutes` ago with its
    /// head committed then too.
    fn new(age_minutes: i64) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let stub = dir.path().join("stub");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&stub).unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, GH_STUB).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let at = iso_minutes_ago(age_minutes);
        let sb = Sandbox {
            _dir: dir,
            bin,
            stub,
            registry: None,
        };
        sb.answer(
            "pr",
            &json!({"headRefOid": HEAD, "createdAt": at, "commits": [{"oid": HEAD, "committedDate": at}]}),
        );
        sb
    }

    /// What `gh` answers for a surface: `pr`, `comments`, `checks` or `reviews`.
    fn answer(&self, surface: &str, body: &Value) {
        fs::write(self.stub.join(format!("{surface}.json")), body.to_string()).unwrap();
    }

    /// What `gh` answers for a surface on its `call`th read only.
    fn answer_on(&self, surface: &str, call: u32, body: &Value) {
        fs::write(
            self.stub.join(format!("{surface}.{call}.json")),
            body.to_string(),
        )
        .unwrap();
    }

    fn with_registry(mut self, registry: &Value) -> Self {
        let p = self.stub.parent().unwrap().join("reviewers.json");
        fs::write(&p, registry.to_string()).unwrap();
        self.registry = Some(p);
        self
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("bash");
        cmd.arg(script())
            .args(args)
            .env("PATH", path)
            .env("REVIEW_STUB", &self.stub)
            .env("REVIEW_WAIT_POLL_SECS", "0");
        match &self.registry {
            Some(p) => cmd.env("REVIEWERS_FILE", p),
            None => cmd.env_remove("REVIEWERS_FILE"),
        };
        cmd.output().expect("spawn bash")
    }
}

fn iso_minutes_ago(minutes: i64) -> String {
    let out = Command::new("date")
        .args(["-u", "+%s"])
        .output()
        .expect("date");
    let now: i64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let t = now - minutes * 60;
    let out = Command::new("jq")
        .args(["-nr", &format!("{t} | todate")])
        .output()
        .expect("jq");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn qodo_comment(sha: &str) -> Value {
    json!({
        "user": {"login": "qodo-code-review[bot]"},
        "html_url": "https://example.test/qodo",
        "body": format!("<h3>Code Review by Qodo</h3>\n...\n<!-- https://github.com/o/r/commit/{sha} -->"),
    })
}

fn greptile_comment() -> Value {
    json!({
        "user": {"login": "greptile-apps[bot]"},
        "html_url": "https://example.test/greptile",
        "body": "<!-- greptile_summary -->\n<h2>Confidence Score: 5/5</h2>",
    })
}

#[test]
fn review_wait_001_both_reviewers_finished_on_the_head_is_complete() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(5);
    sb.answer("comments", &json!([qodo_comment(HEAD), greptile_comment()]));
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert!(out.status.success(), "exit {:?}:\n{text}", out.status);
    assert!(text.contains(&format!("HEAD={HEAD}")), "{text}");
    assert!(
        text.contains("DONE=Qodo https://example.test/qodo"),
        "{text}"
    );
    assert!(
        text.contains("DONE=Greptile https://example.test/greptile"),
        "{text}"
    );
    assert!(text.ends_with("REVIEWS=complete\n"), "{text}");
}

#[test]
fn review_wait_002_a_qodo_review_of_an_older_commit_does_not_count() {
    if !tools_present() {
        return;
    }
    // Opened an hour ago, so every bound has run out and the script decides at
    // once: Greptile's one review covers the PR, Qodo's names an older push.
    let sb = Sandbox::new(60);
    sb.answer("comments", &json!([qodo_comment(OLD), greptile_comment()]));
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "a timeout is a result, not a failure:\n{text}"
    );
    assert!(text.contains("TIMED_OUT=Qodo 20m"), "{text}");
    assert!(text.contains("DONE=Greptile"), "{text}");
    assert!(text.ends_with("REVIEWS=incomplete\n"), "{text}");
}

#[test]
fn review_wait_003_another_account_cannot_stand_in_for_a_reviewer() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(60);
    let mut forged = qodo_comment(HEAD);
    forged["user"]["login"] = json!("someone-else");
    sb.answer("comments", &json!([forged]));
    let text = stdout(&sb.run(&["42"]));
    assert!(text.contains("TIMED_OUT=Qodo"), "{text}");
    assert!(text.contains("TIMED_OUT=Greptile 15m"), "{text}");
    assert!(!text.contains("DONE="), "{text}");
}

#[test]
fn review_wait_004_no_wait_reports_unfinished_reviewers_as_pending() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(1);
    sb.answer("comments", &json!([greptile_comment()]));
    let out = sb.run(&["--no-wait", "42"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("PENDING=Qodo"), "{text}");
    assert!(text.contains("DONE=Greptile"), "{text}");
    assert!(text.ends_with("REVIEWS=incomplete\n"), "{text}");
}

#[test]
fn review_wait_005_it_waits_until_the_review_lands_and_survives_a_failed_read() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(1);
    // First read fails outright, second has only Greptile, third has Qodo on
    // the head as well.
    sb.answer_on("comments", 2, &json!([greptile_comment()]));
    sb.answer_on(
        "comments",
        3,
        &json!([greptile_comment(), qodo_comment(HEAD)]),
    );
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("DONE=Qodo"), "{text}");
    assert!(text.contains("DONE=Greptile"), "{text}");
    assert!(text.ends_with("REVIEWS=complete\n"), "{text}");
}

#[test]
fn review_wait_006_check_run_and_review_signals_key_on_the_head() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(60).with_registry(&json!({"reviewers": [
        {"name": "Checker", "login": "checker[bot]", "timeout_minutes": 5,
         "signal": {"kind": "check_run", "name": "Checker Review"}},
        {"name": "Reviewer", "login": "reviewer[bot]", "timeout_minutes": 5,
         "signal": {"kind": "review"}},
        {"name": "Stale", "login": "stale[bot]", "timeout_minutes": 5,
         "signal": {"kind": "review"}},
    ]}));
    sb.answer(
        "checks",
        &json!({"check_runs": [
            {"name": "Checker Review", "status": "completed", "html_url": "https://example.test/check"}
        ]}),
    );
    sb.answer(
        "reviews",
        &json!([
            {"user": {"login": "reviewer[bot]"}, "state": "COMMENTED", "commit_id": HEAD,
             "html_url": "https://example.test/review"},
            {"user": {"login": "stale[bot]"}, "state": "COMMENTED", "commit_id": OLD},
            {"user": {"login": "stale[bot]"}, "state": "PENDING", "commit_id": HEAD},
        ]),
    );
    let text = stdout(&sb.run(&["42"]));
    assert!(
        text.contains("DONE=Checker https://example.test/check"),
        "{text}"
    );
    assert!(
        text.contains("DONE=Reviewer https://example.test/review"),
        "{text}"
    );
    assert!(
        text.contains("TIMED_OUT=Stale 5m"),
        "an older commit's review and an unsubmitted one are not a review of the head:\n{text}"
    );
}

#[test]
fn review_wait_007_a_malformed_registry_is_an_error_not_a_verdict() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(1).with_registry(&json!({"reviewers": [
        {"name": "Typo", "login": "typo[bot]", "timeout_minutes": 5,
         "signal": {"kind": "coment", "contains": "x"}},
    ]}));
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("ERROR=malformed reviewer"), "{text}");
    assert!(!text.contains("REVIEWS="), "{text}");
}

#[test]
fn review_wait_008_the_real_registry_names_qodo_and_greptile_with_their_signals() {
    let text = fs::read_to_string(repo_root().join(".claude/skills/pr-create/reviewers.json"))
        .expect("read reviewers.json");
    let registry: Value = serde_json::from_str(&text).expect("reviewers.json is JSON");
    let entries = registry["reviewers"].as_array().expect("a reviewers list");
    let find = |login: &str| {
        entries
            .iter()
            .find(|r| r["login"] == login)
            .unwrap_or_else(|| panic!("no entry for {login}"))
    };
    let qodo = find("qodo-code-review[bot]");
    assert_eq!(qodo["signal"]["kind"], "comment");
    assert_eq!(
        qodo["signal"]["names_head"], true,
        "Qodo reviews every push, so only a comment naming the head counts"
    );
    let greptile = find("greptile-apps[bot]");
    assert_eq!(greptile["signal"]["kind"], "comment");
    for r in entries {
        assert!(
            r["note"].as_str().is_some_and(|n| !n.is_empty()),
            "{} needs a note saying where its signal and bound come from",
            r["name"]
        );
    }
}
