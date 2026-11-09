//! Issue #2081: which automated-review findings on a pull request nobody has
//! answered yet.
//!
//! Over 2026-09-10 → 10-28, 22 of 40 `REQUEST_CHANGES` verdicts from the agent
//! reviewer were a bot finding with no reply and no fix at the head, and at
//! least 13 named an item in Qodo's "Code Review by Qodo" summary comment,
//! which is not an inline thread, so the thread-resolution step in `pr-create`
//! never saw it. `.claude/skills/pr-create/unanswered-findings.sh` lists the
//! findings at the head — Qodo's summary items, every tier, and Greptile's —
//! and says which are answered: fixed by Qodo's own judgement, answered in
//! their thread, or named in a PR comment by a person.
//!
//! What it decides is a parse of the two apps' comment shapes plus a rule
//! about replies, so it is driven here against an offline `gh` stand-in, the
//! shape `review_wait.rs` uses, and no test touches the network. The
//! fixtures are cut down from real comments (PRs #2216, #2218).
//!
//! Unix-only: the script needs `bash`, `awk` and `jq`; where `bash` or `jq`
//! cannot be spawned the tests print `SKIP:` and return.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const HEAD: &str = "1111111111111111111111111111111111111111";
const OLD: &str = "2222222222222222222222222222222222222222";
const FINDING_ID: &str = "3055d33c-d094-4567-96f3-f23a5c7ea2a6";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join(".claude/skills/pr-create/unanswered-findings.sh")
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
    eprintln!("SKIP: bash or jq cannot be spawned; the unanswered-findings.sh tests need both");
    false
}

/// An offline `gh`: each surface answers from `$REVIEW_STUB/<surface>.json`,
/// and fails when that file is missing.
const GH_STUB: &str = r#"#!/usr/bin/env bash
answer() {
  if [ -f "$REVIEW_STUB/$1.json" ]; then cat "$REVIEW_STUB/$1.json"
  else echo "HTTP 502" >&2; exit 1
  fi
}
case "$*" in
  "pr view "*) answer pr ;;
  "api repos/{owner}/{repo}/issues/"*"/comments --paginate") answer comments ;;
  "api graphql --paginate "*) answer threads ;;
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
    /// A pull request whose head is `HEAD`, with no comments and no threads.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        let stub = dir.path().join("stub");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&stub).unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, GH_STUB).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let sb = Sandbox {
            _dir: dir,
            bin,
            stub,
            registry: None,
        };
        sb.answer(
            "pr",
            &json!({"headRefOid": HEAD, "url": "https://github.com/o/r/pull/42"}),
        );
        sb.answer("comments", &json!([]));
        sb.threads(vec![]);
        sb
    }

    fn answer(&self, surface: &str, body: &Value) {
        fs::write(self.stub.join(format!("{surface}.json")), body.to_string()).unwrap();
    }

    fn remove(&self, surface: &str) {
        fs::remove_file(self.stub.join(format!("{surface}.json"))).unwrap();
    }

    /// The review threads, as one page of the GraphQL answer.
    fn threads(&self, nodes: Vec<Value>) {
        self.answer(
            "threads",
            &json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": nodes,
            }}}}}),
        );
    }

    fn with_registry(mut self, registry: &Value) -> Self {
        let p = self.stub.parent().unwrap().join("reviewers.json");
        fs::write(&p, registry.to_string()).unwrap();
        self.registry = Some(p);
        self
    }

    fn run(&self) -> Output {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new("bash");
        cmd.arg(script())
            .arg("42")
            .env("PATH", path)
            .env("REVIEW_STUB", &self.stub);
        match &self.registry {
            Some(p) => cmd.env("REVIEWERS_FILE", p),
            None => cmd.env_remove("REVIEWERS_FILE"),
        };
        cmd.output().expect("spawn bash")
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// One open finding in Qodo's summary, in the shape Qodo writes it.
fn qodo_item(n: u32, badge: &str, title: &str) -> String {
    format!(
        r#"<img src="https://img.shields.io/badge/X-634FD1?style=flat-square" height="20px" alt="{badge}">

<details>
<summary>  {n}.  {title} <code>🐞 Bug</code> <code>⚙ Maintainability</code></summary>

<br/>

> <details open>
><summary>Description</summary>
><pre>
>Something is wrong.
></pre>
></details>

[Dismiss ↗](https://app.qodo.ai/findings/{FINDING_ID}?action=dismiss&origin=git) | [View ↗](https://app.qodo.ai/findings/{FINDING_ID})

<hr/>
</details>
"#
    )
}

/// One finding Qodo has marked fixed: struck through, "✓ Resolved", no link.
fn qodo_resolved_item(n: u32, title: &str) -> String {
    format!(
        r#"<details>
<summary>  {n}.  <s>{title}</s> <code>✓ Resolved</code></summary>
<p><b>Resolving commit:</b> <a href="https://github.com/o/r/commit/{OLD}"><code>2222222</code></a></p>
</details>
"#
    )
}

/// Qodo's "Code Review by Qodo" comment for `sha`, with `items` as its current
/// review and `previous` inside its "Previous reviews" section.
fn qodo_comment(sha: &str, items: &str, previous: &str) -> Value {
    json!({
        "user": {"login": "qodo-code-review[bot]", "type": "Bot"},
        "updated_at": "2026-11-09T10:00:00Z",
        "body": format!(
            "<h3>Code Review by Qodo</h3>\n<code>🐞 Bugs (1)</code>\n\n{items}\n\
             <!-- https://github.com/o/r/commit/{sha} -->\n\
             <!-- qodo-previous-reviews:start -->\n<details>\n<summary><strong>Previous reviews</strong></summary>\n\
             {previous}\n<!-- qodo-previous-reviews:end -->"
        ),
    })
}

/// Greptile's summary comment with one finding linked to its thread.
fn greptile_comment(title: &str, thread_root: u64) -> Value {
    json!({
        "user": {"login": "greptile-apps[bot]", "type": "Bot"},
        "updated_at": "2026-11-09T09:00:00Z",
        "body": format!(
            "<!-- greptile_summary -->\n<h2>Confidence Score: 4/5</h2>\n<h2>Findings</h2>\n\n\
             1. <img alt=\"P1\" src=\"https://greptile-static-assets.s3.amazonaws.com/badges/p1.svg?v=9\" align=\"top\">&nbsp;**{title}** \
             <a href=\"https://github.com/o/r/pull/42#discussion_r{thread_root}\">▶</a>\n\n<h3>Summary</h3>\nStuff."
        ),
    })
}

fn person_comment(body: &str) -> Value {
    json!({"user": {"login": "someone", "type": "User"}, "updated_at": "2026-11-09T11:00:00Z", "body": body})
}

fn comment_node(id: u64, login: &str, typename: &str, body: &str) -> Value {
    json!({"databaseId": id, "url": "https://example.test", "body": body,
           "author": {"__typename": typename, "login": login}})
}

fn thread(resolved: bool, comments: Vec<Value>) -> Value {
    json!({"isResolved": resolved, "comments": {"nodes": comments}})
}

/// A thread Qodo opened for its finding `title`, as its inline comment reads.
fn qodo_thread_root(id: u64, title: &str) -> Value {
    comment_node(
        id,
        "qodo-code-review",
        "Bot",
        &format!("<img alt=\"Remediation recommended\">\n\n1\\. {title} <code>🐞 Bug</code>"),
    )
}

/// The real `reviewers.json` with only the named app left in it, so a test
/// about one app is not also told the other never reviewed.
fn registry_with_only(name: &str) -> Value {
    let text = fs::read_to_string(repo_root().join(".claude/skills/pr-create/reviewers.json"))
        .expect("read reviewers.json");
    let mut registry: Value = serde_json::from_str(&text).expect("reviewers.json is JSON");
    registry["reviewers"]
        .as_array_mut()
        .unwrap()
        .retain(|r| r["name"] == name);
    assert_eq!(
        registry["reviewers"].as_array().unwrap().len(),
        1,
        "reviewers.json lists {name}"
    );
    registry
}

#[test]
fn review_findings_001_an_informational_qodo_item_with_no_reply_is_unanswered() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new().with_registry(&registry_with_only("Qodo"));
    let items = qodo_item(
        1,
        "Informational",
        "Returned-offer event still reports an ack-wait expiry",
    );
    sb.answer("comments", &json!([qodo_comment(HEAD, &items, "")]));
    let out = sb.run();
    let text = stdout(&out);
    assert!(out.status.success(), "exit {:?}:\n{text}", out.status);
    assert!(text.starts_with(&format!("HEAD={HEAD}\n")), "{text}");
    assert!(
        text.contains(
            "UNANSWERED=Qodo 1 Informational: Returned-offer event still reports an ack-wait expiry\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with("UNANSWERED_COUNT=1\nFINDINGS=unanswered\n"),
        "{text}"
    );
}

#[test]
fn review_findings_002_an_item_qodo_marks_resolved_is_answered_and_previous_reviews_are_ignored() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new().with_registry(&registry_with_only("Qodo"));
    let current = qodo_resolved_item(1, "Exact PTY pin lacks a reason comment");
    // An open item that exists only in an earlier review is not a finding at the head.
    let previous = qodo_item(1, "Action required", "Something from an older push");
    sb.answer(
        "comments",
        &json!([qodo_comment(HEAD, &current, &previous)]),
    );
    let text = stdout(&sb.run());
    assert!(
        text.contains("ANSWERED=Qodo 1 fixed: Exact PTY pin lacks a reason comment\n"),
        "{text}"
    );
    assert!(!text.contains("older push"), "{text}");
    assert!(text.ends_with("FINDINGS=answered\n"), "{text}");
}

#[test]
fn review_findings_003_a_qodo_thread_counts_once_a_person_replies_or_it_is_resolved() {
    if !tools_present() {
        return;
    }
    let title = "Harness edits rebuild slower than before";
    let items = qodo_item(1, "Remediation recommended", title);

    // A bot's reply in the thread is not an answer.
    let sb = Sandbox::new().with_registry(&registry_with_only("Qodo"));
    sb.answer("comments", &json!([qodo_comment(HEAD, &items, "")]));
    sb.threads(vec![thread(
        false,
        vec![
            qodo_thread_root(10, &title.to_lowercase()),
            comment_node(11, "github-actions", "Bot", "Still open."),
        ],
    )]);
    let text = stdout(&sb.run());
    assert!(
        text.contains("UNANSWERED=Qodo 1 Remediation recommended: "),
        "{text}"
    );

    // A person's reply is.
    sb.threads(vec![thread(
        false,
        vec![
            qodo_thread_root(10, &title.to_lowercase()),
            comment_node(
                12,
                "vfarcic",
                "User",
                "Agreed on the facts; left to the maintainer.",
            ),
        ],
    )]);
    let text = stdout(&sb.run());
    assert!(
        text.contains(&format!("ANSWERED=Qodo 1 thread: {title}\n")),
        "{text}"
    );

    // So is resolving the thread.
    sb.threads(vec![thread(true, vec![qodo_thread_root(10, title)])]);
    let text = stdout(&sb.run());
    assert!(
        text.contains(&format!("ANSWERED=Qodo 1 thread: {title}\n")),
        "{text}"
    );
}

#[test]
fn review_findings_004_a_pr_comment_by_a_person_naming_the_title_or_finding_id_answers_it() {
    if !tools_present() {
        return;
    }
    let items = qodo_item(
        1,
        "Informational",
        "Ack-wait reader list omits the engine&#x27;s tick cadence",
    );
    let sb = Sandbox::new().with_registry(&registry_with_only("Qodo"));

    // A bot quoting the title does not answer it.
    sb.answer(
        "comments",
        &json!([
            qodo_comment(HEAD, &items, ""),
            json!({"user": {"login": "github-actions[bot]", "type": "Bot"},
                   "body": "REQUEST_CHANGES: ack-wait reader list omits the engine's tick cadence"}),
        ]),
    );
    let text = stdout(&sb.run());
    assert!(
        text.contains("UNANSWERED=Qodo 1 Informational: Ack-wait reader list omits the engine's tick cadence\n"),
        "the title is printed with its entities decoded:\n{text}"
    );

    // A person's comment naming it, in other case and with real quotes, does.
    sb.answer(
        "comments",
        &json!([
            qodo_comment(HEAD, &items, ""),
            person_comment("Qodo informational, \"Ack-wait reader list omits the engine's tick cadence\": not changed, because …"),
        ]),
    );
    let text = stdout(&sb.run());
    assert!(text.contains("ANSWERED=Qodo 1 comment: "), "{text}");

    // So does one naming its Qodo finding id.
    sb.answer(
        "comments",
        &json!([
            qodo_comment(HEAD, &items, ""),
            person_comment(&format!("Declined {FINDING_ID}: by design.")),
        ]),
    );
    let text = stdout(&sb.run());
    assert!(text.contains("ANSWERED=Qodo 1 comment: "), "{text}");
}

#[test]
fn review_findings_005_a_greptile_finding_is_answered_in_the_thread_its_summary_links() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new().with_registry(&registry_with_only("Greptile"));
    sb.answer(
        "comments",
        &json!([greptile_comment(
            "Rollback hides running agents",
            4509841945
        )]),
    );
    let root = comment_node(
        4509841945,
        "greptile-apps",
        "Bot",
        "**Rollback hides running agents**",
    );
    let other = comment_node(99, "greptile-apps", "Bot", "**Something else**");
    // A reply in a different thread does not count for this finding.
    sb.threads(vec![
        thread(false, vec![root.clone()]),
        thread(true, vec![other]),
    ]);
    let text = stdout(&sb.run());
    assert!(
        text.contains("UNANSWERED=Greptile 1 P1: Rollback hides running agents\n"),
        "{text}"
    );

    sb.threads(vec![thread(
        false,
        vec![
            root,
            comment_node(100, "vfarcic", "User", "Fixed in abc123."),
        ],
    )]);
    let text = stdout(&sb.run());
    assert!(
        text.contains("ANSWERED=Greptile 1 thread: Rollback hides running agents\n"),
        "{text}"
    );
    assert!(text.ends_with("FINDINGS=answered\n"), "{text}");
}

#[test]
fn review_findings_006_no_review_at_the_head_is_reported_not_passed_silently() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new();
    let items = qodo_item(1, "Action required", "Old finding");
    sb.answer("comments", &json!([qodo_comment(OLD, &items, "")]));
    let out = sb.run();
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("NO_REVIEW=Qodo\n"), "{text}");
    assert!(text.contains("NO_REVIEW=Greptile\n"), "{text}");
    assert!(!text.contains("Old finding"), "{text}");
    assert!(
        text.ends_with("UNANSWERED_COUNT=0\nFINDINGS=answered\n"),
        "{text}"
    );
}

#[test]
fn review_findings_007_an_app_with_no_reader_is_named() {
    if !tools_present() {
        return;
    }
    let mut registry = registry_with_only("Qodo");
    registry["reviewers"][0]["name"] = json!("SomeNewApp");
    let sb = Sandbox::new().with_registry(&registry);
    let text = stdout(&sb.run());
    assert!(text.contains("UNPARSED=SomeNewApp\n"), "{text}");
}

#[test]
fn review_findings_008_a_failed_read_is_an_error_not_a_verdict() {
    if !tools_present() {
        return;
    }
    let sb = Sandbox::new();
    sb.remove("threads");
    let out = sb.run();
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("ERROR=cannot read pull request #42's review threads"),
        "{text}"
    );
    assert!(!text.contains("FINDINGS="), "{text}");
}
