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

/// An offline `gh`. Each surface the script reads fails on its `<call>`th read
/// when `$REVIEW_STUB/<surface>.<call>.fail` exists, and otherwise answers from
/// `<surface>.<call>.json` when that file exists, else from `<surface>.json`,
/// failing when neither does — so a test can change the answer between polls,
/// or make a read fail.
const GH_STUB: &str = r#"#!/usr/bin/env bash
answer() {
  local n
  n=$(( $(cat "$REVIEW_STUB/$1.count" 2>/dev/null || echo 0) + 1 ))
  echo "$n" >"$REVIEW_STUB/$1.count"
  if [ -f "$REVIEW_STUB/$1.$n.fail" ]; then echo "HTTP 502" >&2; exit 1
  elif [ -f "$REVIEW_STUB/$1.$n.json" ]; then cat "$REVIEW_STUB/$1.$n.json"
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
    /// A pull request whose head is `HEAD`, opened `age_minutes` ago.
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
        sb.answer("pr", &json!({"headRefOid": HEAD, "createdAt": at}));
        sb
    }

    /// The PR as `gh pr view` answers on its `call`th read only, with `head`
    /// as its head commit.
    fn pr_on(&self, call: u32, head: &str) {
        let at = iso_minutes_ago(1);
        self.answer_on("pr", call, &json!({"headRefOid": head, "createdAt": at}));
    }

    /// Make a surface's `call`th read fail.
    fn fail_on(&self, surface: &str, call: u32) {
        fs::write(self.stub.join(format!("{surface}.{call}.fail")), "").unwrap();
    }

    /// How many times the script read a surface.
    fn reads(&self, surface: &str) -> u32 {
        fs::read_to_string(self.stub.join(format!("{surface}.count")))
            .map(|s| s.trim().parse().unwrap_or(0))
            .unwrap_or(0)
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

/// The real `reviewers.json` with every bound cut to nothing, so a test about
/// what does NOT count decides at once instead of waiting out 20 minutes.
fn real_registry_without_bounds() -> Value {
    let mut registry = real_registry();
    for r in registry["reviewers"].as_array_mut().unwrap() {
        r["timeout_minutes"] = json!(0.001);
    }
    registry
}

fn real_registry() -> Value {
    let text = fs::read_to_string(repo_root().join(".claude/skills/pr-create/reviewers.json"))
        .expect("read reviewers.json");
    serde_json::from_str(&text).expect("reviewers.json is JSON")
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
    let sb = Sandbox::new(5).with_registry(&real_registry_without_bounds());
    sb.answer("comments", &json!([qodo_comment(OLD), greptile_comment()]));
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "a timeout is a result, not a failure:\n{text}"
    );
    assert!(text.contains("TIMED_OUT=Qodo"), "{text}");
    assert!(text.contains("DONE=Greptile"), "{text}");
    assert!(text.ends_with("REVIEWS=incomplete\n"), "{text}");
}

#[test]
fn review_wait_003_another_account_cannot_stand_in_for_a_reviewer() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(5).with_registry(&real_registry_without_bounds());
    let mut forged = qodo_comment(HEAD);
    forged["user"]["login"] = json!("someone-else");
    sb.answer("comments", &json!([forged]));
    let text = stdout(&sb.run(&["42"]));
    assert!(text.contains("TIMED_OUT=Qodo"), "{text}");
    assert!(text.contains("TIMED_OUT=Greptile"), "{text}");
    assert!(!text.contains("DONE="), "{text}");
}

/// Greptile's finding on PR #1882: a bound counted from the head commit's date
/// ran out at once for an old commit pushed late. And Qodo's: a bound counted
/// from the head gave a once-at-open reviewer a fresh wait on every push.
#[test]
fn review_wait_004_each_bound_counts_from_what_that_reviewer_reviews() {
    if !tools_present() {
        return;
    }
    // Opened an hour ago, no review from either app yet.
    let sb = Sandbox::new(60);
    sb.answer("comments", &json!([]));
    let out = sb.run(&["--no-wait", "42"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("PENDING=Qodo"),
        "Qodo reviews every push, so its bound starts with the wait, however old the PR is:\n{text}"
    );
    assert!(
        text.contains("TIMED_OUT=Greptile 15m"),
        "Greptile reviews once, so its bound counts from the PR opening an hour ago:\n{text}"
    );
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

/// Qodo's finding on PR #1882: a push during the wait used to leave the verdict
/// about the head the wait started on.
#[test]
fn review_wait_006_a_push_during_the_wait_moves_the_verdict_to_the_new_head() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(1);
    // The wait starts on OLD, which Qodo has already reviewed; by the time it
    // checks before deciding, HEAD has been pushed. Qodo reviews HEAD on the
    // third read of the comments.
    sb.pr_on(1, OLD);
    sb.answer_on(
        "comments",
        1,
        &json!([qodo_comment(OLD), greptile_comment()]),
    );
    sb.answer_on(
        "comments",
        2,
        &json!([qodo_comment(OLD), greptile_comment()]),
    );
    sb.answer("comments", &json!([qodo_comment(HEAD), greptile_comment()]));
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains(&format!("HEAD={HEAD}")), "{text}");
    assert!(
        !text.contains(OLD),
        "the verdict is about the new head:\n{text}"
    );
    assert!(text.contains("DONE=Qodo"), "{text}");
    assert!(text.ends_with("REVIEWS=complete\n"), "{text}");
    assert_eq!(
        sb.reads("comments"),
        3,
        "Qodo's review of the old head must not end the wait for the new one"
    );
}

/// Qodo's finding on PR #1882's fix: a failed read just before the verdict was
/// taken as "the head has not moved".
#[test]
fn review_wait_010_a_failed_read_before_the_verdict_is_retried_not_trusted() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(1);
    // The wait starts on OLD, already reviewed. The read before the verdict
    // fails, the retry finds HEAD, and Qodo reviews HEAD on the third read.
    sb.pr_on(1, OLD);
    sb.fail_on("pr", 2);
    sb.answer_on(
        "comments",
        1,
        &json!([qodo_comment(OLD), greptile_comment()]),
    );
    sb.answer_on(
        "comments",
        2,
        &json!([qodo_comment(OLD), greptile_comment()]),
    );
    sb.answer("comments", &json!([qodo_comment(HEAD), greptile_comment()]));
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains(&format!("HEAD={HEAD}")), "{text}");
    assert!(text.ends_with("REVIEWS=complete\n"), "{text}");
    assert_eq!(sb.reads("comments"), 3, "{text}");
}

#[test]
fn review_wait_011_a_head_that_cannot_be_confirmed_gets_no_verdict() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(1);
    sb.answer("comments", &json!([qodo_comment(HEAD), greptile_comment()]));
    for call in 2..=4 {
        sb.fail_on("pr", call);
    }
    let out = sb.run(&["42"]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("ERROR=cannot read pull request #42"),
        "{text}"
    );
    assert!(!text.contains("REVIEWS="), "{text}");
}

/// Greptile's finding on PR #1882: a check-run was matched by name alone.
#[test]
fn review_wait_007_check_run_and_review_signals_key_on_the_head_and_the_app() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new(5).with_registry(&json!({"reviewers": [
        {"name": "Checker", "login": "checker[bot]", "timeout_minutes": 0.001,
         "signal": {"kind": "check_run", "name": "Checker Review", "app": "checker"}},
        {"name": "Copycat", "login": "copycat[bot]", "timeout_minutes": 0.001,
         "signal": {"kind": "check_run", "name": "Copycat Review", "app": "copycat"}},
        {"name": "Reviewer", "login": "reviewer[bot]", "timeout_minutes": 0.001,
         "signal": {"kind": "review"}},
        {"name": "Stale", "login": "stale[bot]", "timeout_minutes": 0.001,
         "signal": {"kind": "review"}},
    ]}));
    sb.answer(
        "checks",
        &json!({"check_runs": [
            {"name": "Checker Review", "status": "completed", "app": {"slug": "checker"},
             "html_url": "https://example.test/check"},
            {"name": "Copycat Review", "status": "completed", "app": {"slug": "someone-else"}}
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
        text.contains("TIMED_OUT=Copycat"),
        "a same-named check from another app is not that app's review:\n{text}"
    );
    assert!(
        text.contains("DONE=Reviewer https://example.test/review"),
        "{text}"
    );
    assert!(
        text.contains("TIMED_OUT=Stale"),
        "an older commit's review and an unsubmitted one are not a review of the head:\n{text}"
    );
}

#[test]
fn review_wait_008_a_malformed_registry_is_an_error_not_a_verdict() {
    if !tools_present() {
        return;
    }
    for (what, entry) in [
        (
            "an unknown signal",
            json!({"name": "Typo", "login": "typo[bot]", "timeout_minutes": 5,
                   "signal": {"kind": "coment", "contains": "x"}}),
        ),
        (
            "a check-run with no app",
            json!({"name": "Anyone", "login": "anyone[bot]", "timeout_minutes": 5,
                   "signal": {"kind": "check_run", "name": "Review"}}),
        ),
    ] {
        let sb = Sandbox::new(1).with_registry(&json!({ "reviewers": [entry] }));
        let out = sb.run(&["42"]);
        let text = stdout(&out);
        assert_eq!(out.status.code(), Some(2), "{what}:\n{text}");
        assert!(text.contains("ERROR=malformed reviewer"), "{what}:\n{text}");
        assert!(!text.contains("REVIEWS="), "{what}:\n{text}");
    }
}

#[test]
fn review_wait_009_the_real_registry_names_qodo_and_greptile_with_their_signals() {
    let registry = real_registry();
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
    assert_ne!(
        greptile["signal"]["names_head"], true,
        "Greptile reviews once, when the PR opens, so its one comment covers the PR"
    );
    for r in entries {
        assert!(
            r["note"].as_str().is_some_and(|n| !n.is_empty()),
            "{} needs a note saying where its signal and bound come from",
            r["name"]
        );
    }
}
