//! The adaptive issue labeler's `Extract label policy for the agent` setup step.
//!
//! The step parses this repository's own workflow frontmatter and writes the
//! `add-labels` allow/block lists to `/tmp/gh-aw/agent/label-policy.json`, which
//! the prompt tells the agent to read at the start of every run — and *not* to
//! fall back to reading the workflow source unless that file is missing.
//!
//! It had never once produced that file. Its search was `^\s*allowed:` over the
//! whole frontmatter, and three keys there are spelled `allowed:`: `network`'s
//! `[defaults]` at the top, the tools list next, and only then the label list.
//! So it matched `[defaults]`, failed to parse it as JSON, called `bail()` —
//! which prints and **exits 0** — and every run silently took the fallback path
//! the prompt reserves for a missing file. Nothing went red, because nothing was
//! ever asserted about the step's output.
//!
//! These tests drive the **real** script under `python3`, extracted from the
//! Markdown the same way `issue_labeler_memory` extracts the `node` validator,
//! and cover the shape of the original bug rather than only the happy path: the
//! decoy `allowed:` keys must lose to the label list even though they come
//! first.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Human-edited agentic-workflow source, and the file the step itself parses.
const WORKFLOW_MD: &str = ".github/workflows/issue-labeler.md";

/// The job step whose `run:` body is under test.
const STEP_NAME: &str = "Extract label policy for the agent";

/// The output directory the script hard-codes because a GitHub runner
/// guarantees it. It appears twice — the `makedirs` and the path itself.
const AGENT_DIR_LITERAL: &str = "/tmp/gh-aw/agent";

fn read_lf(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

/// `python3` is on every GitHub runner and in this repo's devbox. Where it is
/// absent, say so loudly rather than failing a contributor's unrelated change.
fn python_present() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Pull the step's `python3 <<'SCRIPT' … SCRIPT` heredoc out of the Markdown
/// and undo its six-space block indent.
fn policy_source() -> String {
    let md = read_lf(&repo_root().join(WORKFLOW_MD));
    let step = md
        .find(&format!("- name: {STEP_NAME}"))
        .unwrap_or_else(|| panic!("{WORKFLOW_MD}: no `{STEP_NAME}` step"));
    let open = "\n      python3 <<'SCRIPT'\n";
    let close = "\n      SCRIPT\n";
    let body_start = md[step..]
        .find(open)
        .map(|i| step + i + open.len())
        .unwrap_or_else(|| panic!("{WORKFLOW_MD}: `{STEP_NAME}` has no python3 heredoc"));
    let body_end = md[body_start..]
        .find(close)
        .map(|i| body_start + i)
        .unwrap_or_else(|| panic!("{WORKFLOW_MD}: `{STEP_NAME}` heredoc is unterminated"));
    md[body_start..body_end]
        .lines()
        .map(|l| l.strip_prefix("      ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the step did with a given workspace.
struct Outcome {
    stdout: String,
    /// Parsed `label-policy.json`, or `None` if the step never wrote it.
    policy: Option<serde_json::Value>,
}

/// Run the real script against a workspace containing `workflow` as
/// `.github/workflows/issue-labeler.md`, with its output redirected into a
/// `TempDir`. `None` means the environment cannot run it at all.
fn run_against(workflow: &str) -> Option<Outcome> {
    if !python_present() {
        eprintln!("SKIP: issue-labeler label-policy tests need `python3` on PATH");
        return None;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("ws");
    let workflows = workspace.join(".github").join("workflows");
    std::fs::create_dir_all(&workflows).expect("workflows dir");
    std::fs::write(workflows.join("issue-labeler.md"), workflow).expect("workflow");

    let out_dir = tmp.path().join("agent");
    let script = policy_source();
    // Both occurrences are inside Python string literals, and a Windows temp
    // path is full of backslashes that would otherwise read as escapes.
    let hits = script.matches(AGENT_DIR_LITERAL).count();
    assert_eq!(
        hits, 2,
        "expected exactly two `{AGENT_DIR_LITERAL}` in the step, found {hits} — the workflow \
         moved it and this test would no longer sandbox the write"
    );
    let script = script.replace(
        AGENT_DIR_LITERAL,
        &out_dir
            .to_str()
            .expect("utf-8 temp path")
            .replace('\\', "\\\\"),
    );
    let script_path = tmp.path().join("policy.py");
    std::fs::write(&script_path, script).expect("script");

    let out = Command::new("python3")
        .arg(&script_path)
        .env("GITHUB_WORKSPACE", &workspace)
        .env("WORKFLOW_NAME", "issue-labeler")
        .output()
        .expect("python3");
    assert!(
        out.status.success(),
        "the step is written to exit 0 on every path, but it failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let written = out_dir.join("label-policy.json");
    Some(Outcome {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        policy: std::fs::read_to_string(&written)
            .ok()
            .map(|t| serde_json::from_str(&t).expect("label-policy.json is not valid JSON")),
    })
}

/// Every label in the frontmatter's `add-labels.allowed`, read independently of
/// the step so the assertion is against the source of truth rather than against
/// the step's own idea of it.
fn frontmatter_list(md: &str, key: &str) -> Vec<String> {
    let block = md
        .find("\n  add-labels:\n")
        .map(|i| &md[i..])
        .expect("no add-labels: block");
    let line = block
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{key}: [")))
        .unwrap_or_else(|| panic!("no {key}: line in the add-labels: block"));
    let inner = line
        .split_once('[')
        .and_then(|(_, r)| r.rsplit_once(']'))
        .expect("bracketed list")
        .0;
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The step must produce the file the prompt is written around, carrying the
/// label taxonomy — not the `network:` or tools list that precede it.
///
/// This is the regression test proper: run against the repository's own
/// workflow, which is where the three competing `allowed:` keys actually live.
#[test]
fn label_policy_is_extracted_from_the_add_labels_block() {
    let md = read_lf(&repo_root().join(WORKFLOW_MD));
    let Some(out) = run_against(&md) else { return };

    let policy = out.policy.unwrap_or_else(|| {
        panic!(
            "the step wrote no label-policy.json, so the agent silently falls back to reading \
             the workflow source — which the prompt reserves for the file being missing. \
             It said:\n{}",
            out.stdout
        )
    });

    for key in ["allowed", "blocked"] {
        let expected = frontmatter_list(&md, key);
        let actual: Vec<String> = policy[key]
            .as_array()
            .unwrap_or_else(|| panic!("label-policy.json has no `{key}` array"))
            .iter()
            .map(|v| v.as_str().expect("string label").to_string())
            .collect();
        assert_eq!(
            actual, expected,
            "label-policy.json's `{key}` is not the add-labels `{key}:` list"
        );
    }

    let allowed = frontmatter_list(&md, "allowed");
    assert!(
        allowed.contains(&"daemon".to_string()),
        "sanity: the taxonomy should carry the component labels, got {allowed:?}"
    );
    assert!(
        !allowed.contains(&"defaults".to_string()),
        "the step picked up `network:`'s `allowed: [defaults]` — the original bug"
    );
}

/// The decoys are the whole reason this can regress, so pin them directly: a
/// frontmatter whose *only* parseable `allowed:` before the label list is a
/// non-JSON one must still yield the label list. An unanchored search matches
/// the first `allowed:` and bails on it, which is exactly what shipped.
#[test]
fn label_policy_ignores_allowed_keys_outside_the_add_labels_block() {
    let md = "\
---
network:
  allowed: [defaults]
tools:
  github:
    allowed: [issue_read, pull_request_read]
safe-outputs:
  add-labels:
    max: 6
    allowed: [\"bug\", \"tui\"]
    blocked: [\"PRD\"]
  noop:
    report-as-issue: false
---

# body
";
    let Some(out) = run_against(md) else { return };
    let policy = out
        .policy
        .unwrap_or_else(|| panic!("no label-policy.json written; step said:\n{}", out.stdout));
    assert_eq!(policy["allowed"], serde_json::json!(["bug", "tui"]));
    assert_eq!(policy["blocked"], serde_json::json!(["PRD"]));
}

/// The step is deliberately non-fatal — `bail()` prints and exits 0 — because a
/// missing policy file has a documented fallback and must not fail the run. A
/// frontmatter with no `add-labels:` block at all is the case that exercises it.
#[test]
fn label_policy_bails_without_failing_the_run() {
    let md = "\
---
network:
  allowed: [defaults]
---

# body
";
    let Some(out) = run_against(md) else { return };
    assert!(
        out.policy.is_none(),
        "a frontmatter with no add-labels: block must not yield a policy file"
    );
    assert!(
        out.stdout.contains("label policy:"),
        "the step must say why it bailed, so the fallback is visible in the run log; \
         it printed:\n{}",
        out.stdout
    );
}
