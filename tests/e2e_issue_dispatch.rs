#![cfg(feature = "e2e")]

//! L2 GitHub-dispatch tests for the daemon-hosted scheduler (PRD #120).
//!
//! These drive the same headless `dot-agent-deck daemon serve` harness as the
//! `scheduler/spawn/*` family (run-now over the attach socket, no PTY), but
//! exercise the NEW `issue_dispatch` task type: on fire the daemon must
//! enumerate a repo's open issues via `gh`, provision the repo (clone), create a
//! per-issue worktree on `agent/issue-<n>`, dedup already-claimed issues, and
//! spawn one agent per remaining issue into its worktree — reusing #127's spawn
//! primitive (orchestration tab vs single-agent card).
//!
//! Everything runs OFFLINE behind the TESTABILITY SEAM: a stub `gh` script on
//! PATH branches on argv (`issue list` → canned JSON, `pr list` → canned JSON,
//! `repo clone` → `git clone` of a LOCAL fixture remote). The fixture remote is
//! a real one-commit git repo (optionally carrying a committed
//! `.dot-agent-deck.toml` so the dispatched worktree opens an orchestration tab).
//!
//! Implemented now (dispatch/001-009 all GREEN): firing an `issue_dispatch` task
//! runs the full GitHub-dispatch flow — the daemon clones the repo, derives a
//! per-issue worktree on `agent/issue-<n>`, dedups issues that already carry an
//! open PR or a live worktree, and spawns one agent per remaining issue into its
//! worktree. Each test asserts that observable end-state — clone present,
//! worktree created, per-issue agent(s) rooted in it — across the seam above.
//!
//! PRD #120 FINAL DECISION: the `experimental` flag gates only the CREATION UX
//! (the new-pane `schedule: issues` option and the issue-dispatch authoring
//! form), NOT the dispatch BEHAVIOR. A configured `issue_dispatch` task fires
//! regardless of the flag, so these daemon-spawn envs carry no
//! `DOT_AGENT_DECK_EXPERIMENTAL`. (The former `dispatch/010`, which asserted the
//! flow stayed inert with the flag off, was removed when the behavior gate went
//! away.) Flag-gated creation UX is covered by the `scheduler/cli`,
//! `new-pane/issue-dispatch`, and `scheduler/manager` catalog families.

mod common;

#[path = "support/inbox_view.rs"]
mod view;

use std::path::{Path, PathBuf};
use std::time::Duration;

use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::issue_dispatch::derive_issue_paths;
use spec::spec;

const CLAIM_MARKER: &str = "<!-- dot-agent-deck:claim -->";

// ---------------------------------------------------------------------------
// Stub `gh` + local fixture-remote harness (the TESTABILITY SEAM)
// ---------------------------------------------------------------------------

/// Offline GitHub seam: argv is logged before validation, writes persist repo
/// state, and unknown invocations fail. A missing label fails issue-edit just
/// as gh's label-name resolution does; `read-only` denies each write verb.
const GH_STUB_SCRIPT: &str = r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

args = sys.argv[1:]
root = Path(os.environ['GHSTUB_DIR'])
with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\n')

def fail(message):
    print(message, file=sys.stderr)
    sys.exit(1)

def option(name):
    return args[args.index(name) + 1]

def emit(value):
    print(json.dumps(value))
    sys.exit(0)

def hang_if_enabled(paths):
    # A file-controlled long call, without a child sleep process to leak.
    deadline = time.monotonic() + 120
    while any(path.exists() for path in paths) and time.monotonic() < deadline:
        time.sleep(0.05)

if args == ['api', 'user']:
    hang_if_enabled([root / 'hang-user'])
    emit(json.loads((root / 'authenticated-user.json').read_text()))

if args[:2] == ['repo', 'clone'] and len(args) == 4:
    remote = root / args[2].replace('/', '_') / 'remote'
    sys.exit(subprocess.call(['git', 'clone', '--quiet', str(remote), args[3]]))

if args[:2] in (['issue', 'list'], ['pr', 'list']):
    rd = root / option('--repo').replace('/', '_')
    if args[:2] == ['issue', 'list']:
        emit(json.loads((rd / 'issues.json').read_text()))
    n = option('--head').rsplit('-', 1)[-1]
    if (rd / ('fail-pr-' + n)).exists():
        fail('gh: simulated API error for issue ' + n + ' (agent/issue-' + n + ')')
    data = rd / ('pr-' + n + '.json')
    emit(json.loads(data.read_text()) if data.exists() else [])

kind = None
if (len(args) == 10 and args[:3] == ['label', 'create', 'agent-deck/claimed']
        and args[3] == '--repo' and args[5:7] == ['--force', '--color']
        and re.fullmatch('[0-9a-fA-F]{6}', args[7]) and args[8] == '--description'):
    repo, kind = args[4], 'label'
elif (len(args) == 8 and args[:3] == ['issue', 'edit', '--repo']
        and args[4:7] == ['--add-label', 'agent-deck/claimed', '--']
        and args[7].isdigit()):
    repo, issue, kind = args[3], args[7], 'issue'
elif args[:2] == ['api', '--paginate'] and len(args) == 3:
    match = re.fullmatch(r'repos/([^/]+/[^/]+)/issues/(\d+)/comments', args[2])
    if match:
        repo, issue = match.groups()
        kind = 'list'
elif (len(args) == 6 and args[:2] == ['api', '--method']
        and args[4] == '-f' and args[5].startswith('body=')):
    if args[2] == 'POST':
        match = re.fullmatch(r'repos/([^/]+/[^/]+)/issues/(\d+)/comments', args[3])
        if match:
            repo, issue = match.groups()
            kind = 'POST'
    elif args[2] == 'PATCH':
        match = re.fullmatch(r'repos/([^/]+/[^/]+)/issues/comments/(\d+)', args[3])
        if match:
            repo, comment_id = match.groups()
            kind = 'PATCH'
if kind is None:
    with (root / 'unhandled.jsonl').open('a') as log:
        log.write(json.dumps(args) + '\n')
    fail('gh stub: unhandled invocation: ' + repr(args))

rd = root / repo.replace('/', '_')
hang_paths = [rd / 'hang', rd / ('hang-' + kind)]
if kind in ('issue', 'list', 'POST'):
    hang_paths += [rd / ('hang-issue-' + issue), rd / ('hang-issue-' + issue + '-' + kind)]
hang_if_enabled(hang_paths)
state_file = rd / 'state.json'
state = json.loads(state_file.read_text())
if kind == 'list':
    emit(state['comments'].get(issue, []))
if (rd / 'read-only').exists():
    fail('HTTP 403: Resource not accessible by integration (' + kind + ')')
if kind == 'label':
    if (rd / 'fail-provision').exists():
        fail('HTTP 403: token may triage issues but cannot create labels')
    state['labels'] = ['agent-deck/claimed']
elif kind == 'issue':
    if 'agent-deck/claimed' not in state['labels']:
        fail("could not add label: 'agent-deck/claimed' not found")
    state['issue_labels'][issue] = ['agent-deck/claimed']
elif kind == 'POST':
    comment = {'id': state['next_id'], 'body': args[5][5:],
               'user': json.loads((root / 'authenticated-user.json').read_text()),
               'created_at': '2026-10-22T00:00:00Z', 'updated_at': '2026-10-22T00:00:00Z'}
    state['next_id'] += 1
    state['comments'].setdefault(issue, []).append(comment)
elif kind == 'PATCH':
    found = [c for comments in state['comments'].values() for c in comments
             if c['id'] == int(comment_id)]
    if len(found) != 1:
        fail('HTTP 404: Not Found')
    found[0]['body'] = args[5][5:]
    found[0]['updated_at'] = '2026-10-22T00:00:01Z'
temporary = state_file.with_suffix('.tmp')
temporary.write_text(json.dumps(state))
temporary.replace(state_file)
emit(comment if kind == 'POST' else found[0] if kind == 'PATCH' else {})
"#;

/// A committed `.dot-agent-deck.toml` whose single-role orchestration runs `cat`
/// (which echoes the delivered prompt) — mirrors `scheduler/spawn/002`'s
/// orchestration fixture so a dispatched worktree opens an orchestration tab.
const ORCH_TOML: &str = "[[orchestrations]]\nname = \"dispatch-orch\"\n\n\
     [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n";

/// A committed `.dot-agent-deck.toml` whose orchestration has TWO roles
/// (`orchestrator` + `reviewer`, both `cat`). A dispatched worktree opens an
/// orchestration tab with two role panes that share ONE `orchestration_cwd` (the
/// issue worktree) — the fixture for `scheduler/dispatch/009`'s refcount check.
const MULTIROLE_ORCH_TOML: &str = "[[orchestrations]]\nname = \"dispatch-orch\"\n\n\
     [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
     [[orchestrations.roles]]\nname = \"reviewer\"\ncommand = \"cat\"\n";

/// Holds the stub `gh` + the per-repo fixture data dir, both rooted in a scratch
/// tempdir kept alive for the test's lifetime.
struct GhStub {
    _scratch: tempfile::TempDir,
    /// `$GHSTUB_DIR` — per-repo canned data lives under here.
    dir: PathBuf,
    /// Dir holding the `gh` script; prepended to the daemon's PATH.
    bindir: PathBuf,
}

impl GhStub {
    fn new() -> Self {
        let scratch = common::harness_tempdir().expect("gh stub scratch tempdir");
        let base = scratch.path().to_path_buf();
        let dir = base.join("ghstub");
        let bindir = base.join("bin");
        std::fs::create_dir_all(&dir).expect("create ghstub data dir");
        std::fs::create_dir_all(&bindir).expect("create ghstub bin dir");
        std::fs::write(
            dir.join("authenticated-user.json"),
            r#"{"id":12345,"login":"deck-bot"}"#,
        )
        .expect("seed authenticated GitHub account");
        let gh = bindir.join("gh");
        std::fs::write(&gh, GH_STUB_SCRIPT).expect("write gh stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))
                .expect("chmod gh stub");
        }
        GhStub {
            _scratch: scratch,
            dir,
            bindir,
        }
    }

    fn key(repo: &str) -> String {
        repo.replace('/', "_")
    }

    fn repo_dir(&self, repo: &str) -> PathBuf {
        self.dir.join(Self::key(repo))
    }

    /// Create the fixture remote for `repo` (a real one-commit git repo). When
    /// `with_orchestration`, the commit carries `.dot-agent-deck.toml`.
    fn add_repo(&self, repo: &str, with_orchestration: bool) {
        let rd = self.repo_dir(repo);
        std::fs::create_dir_all(&rd).expect("create repo fixture dir");
        init_remote(&rd.join("remote"), with_orchestration);
        self.seed_state(repo);
    }

    /// Like [`add_repo`] but the committed `.dot-agent-deck.toml` defines a
    /// TWO-role orchestration (orchestrator + reviewer, both `cat`), so a
    /// dispatched worktree opens an orchestration tab with two role panes that
    /// share the SAME issue worktree as their `orchestration_cwd`.
    fn add_repo_multirole(&self, repo: &str) {
        let rd = self.repo_dir(repo);
        std::fs::create_dir_all(&rd).expect("create repo fixture dir");
        init_remote_with_orch_toml(&rd.join("remote"), Some(MULTIROLE_ORCH_TOML));
        self.seed_state(repo);
    }

    fn seed_state(&self, repo: &str) {
        std::fs::write(
            self.repo_dir(repo).join("state.json"),
            r#"{"labels":[],"issue_labels":{},"comments":{},"next_id":101}"#,
        )
        .expect("seed repo without labels or claim comments");
    }

    fn read_only(&self, repo: &str) {
        std::fs::write(self.repo_dir(repo).join("read-only"), "").expect("deny GitHub writes");
    }

    fn set_authenticated_user(&self, id: u64, login: &str) {
        std::fs::write(
            self.dir.join("authenticated-user.json"),
            serde_json::json!({"id": id, "login": login}).to_string(),
        )
        .expect("configure authenticated GitHub account");
    }

    fn seed_comment(&self, repo: &str, issue: u64, mut comment: serde_json::Value) -> u64 {
        let mut state = self.state(repo);
        let id = state["next_id"].as_u64().unwrap();
        comment["id"] = id.into();
        state["next_id"] = (id + 1).into();
        state["comments"][issue.to_string()] = serde_json::json!([comment]);
        std::fs::write(self.repo_dir(repo).join("state.json"), state.to_string())
            .expect("seed foreign claim comment");
        id
    }

    fn deny_provision_existing_label(&self, repo: &str) {
        let mut state = self.state(repo);
        state["labels"] = serde_json::json!(["agent-deck/claimed"]);
        std::fs::write(self.repo_dir(repo).join("state.json"), state.to_string())
            .expect("seed existing label");
        std::fs::write(self.repo_dir(repo).join("fail-provision"), "")
            .expect("deny label provisioning but allow issue triage");
    }

    fn hang_issue_label(&self, repo: &str, issue: u64) {
        std::fs::write(
            self.repo_dir(repo)
                .join(format!("hang-issue-{issue}-issue")),
            "",
        )
        .expect("hang only this issue's label write");
    }

    fn state(&self, repo: &str) -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(self.repo_dir(repo).join("state.json")).expect("read gh state"),
        )
        .expect("valid gh state")
    }

    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.dir.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("logged gh argv"))
            .collect()
    }

    fn assert_no_unhandled_calls(&self) {
        assert!(
            !self.dir.join("unhandled.jsonl").exists(),
            "gh received an unrecognized invocation: {:?}",
            std::fs::read_to_string(self.dir.join("unhandled.jsonl"))
        );
    }

    fn marker_comments(&self, repo: &str, issue: u64) -> Vec<serde_json::Value> {
        self.state(repo)["comments"][issue.to_string()]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| {
                c["body"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(CLAIM_MARKER)
            })
            .cloned()
            .collect()
    }

    /// Set the canned `gh issue list` output for `repo` (issue numbers, in
    /// returned order).
    fn set_issues(&self, repo: &str, numbers: &[u64]) {
        let body = numbers
            .iter()
            .map(|n| format!("{{\"number\":{n}}}"))
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(
            self.repo_dir(repo).join("issues.json"),
            format!("[{body}]\n"),
        )
        .expect("write issues.json");
    }

    /// Make `gh pr list --head agent/issue-<n>` report an open PR (skip signal).
    fn set_open_pr(&self, repo: &str, issue: u64) {
        std::fs::write(
            self.repo_dir(repo).join(format!("pr-{issue}.json")),
            "[{\"number\":4242}]\n",
        )
        .expect("write pr-<n>.json");
    }

    /// Make `gh pr list --head agent/issue-<n>` exit non-zero (a simulated
    /// per-issue GitHub error).
    fn fail_pr(&self, repo: &str, issue: u64) {
        std::fs::write(self.repo_dir(repo).join(format!("fail-pr-{issue}")), b"")
            .expect("write fail-pr-<n> marker");
    }

    /// PATH with the stub `gh` dir first, so the daemon's `gh` resolves here
    /// while `git` still comes from the real PATH.
    fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.bindir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn ghstub_dir(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }
}

/// Initialize a fixture remote: a real git repo with one commit (`README.md`,
/// plus the single-role [`ORCH_TOML`] `.dot-agent-deck.toml` when
/// `with_orchestration`). Thin wrapper over [`init_remote_with_orch_toml`].
fn init_remote(remote: &Path, with_orchestration: bool) {
    init_remote_with_orch_toml(remote, with_orchestration.then_some(ORCH_TOML));
}

/// Initialize a fixture remote committing `README.md` plus, when `orch_toml` is
/// `Some`, that exact `.dot-agent-deck.toml` content (so callers can choose a
/// single- or multi-role orchestration config). Commit identity is pinned inline
/// so the repo does not depend on the host's global git config.
fn init_remote_with_orch_toml(remote: &Path, orch_toml: Option<&str>) {
    std::fs::create_dir_all(remote).expect("create remote dir");
    run_git(remote, &["-c", "init.defaultBranch=main", "init", "-q"]);
    std::fs::write(remote.join("README.md"), "issue-dispatch fixture\n").expect("write README");
    if let Some(toml) = orch_toml {
        std::fs::write(remote.join(".dot-agent-deck.toml"), toml)
            .expect("write fixture .dot-agent-deck.toml");
    }
    run_git(remote, &["add", "-A"]);
    run_git(remote, &["commit", "-q", "-m", "init"]);
}

/// Through `common::fixture_git`, so the ambient git LOCATION variables are
/// cleared: a bare `git` with only `.current_dir` resolves an ambient `GIT_DIR`
/// in preference to it, which makes `init`/`add`/`commit` here writes against
/// whatever that names (issue #834). It also supplies the commit identity by
/// environment and neutralizes the host's config, which is what replaced the
/// `-c user.email=… -c user.name=… -c commit.gpgsign=false` this file used to
/// thread through the one `commit`. `dir` is its own sandbox root — every
/// caller passes a repository root.
fn run_git(dir: &Path, args: &[&str]) {
    let status = common::fixture_git(dir, dir)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

// ---------------------------------------------------------------------------
// schedules.toml builder
// ---------------------------------------------------------------------------

/// One `[[scheduled_tasks]]` block with an `[scheduled_tasks.issue_dispatch]`
/// sub-table (no top-level `command` — the per-issue command comes from each
/// cloned repo's config). The cron never fires on its own; fires are driven via
/// run-now.
fn dispatch_task(
    name: &str,
    working_dir: &str,
    prompt: &str,
    repo: &str,
    max_per_run: usize,
) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"{name}\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{working_dir}\"\n\
         prompt = \"{prompt}\"\n\
         enabled = true\n\
         \n\
         [scheduled_tasks.issue_dispatch]\n\
         repo = \"{repo}\"\n\
         max_per_run = {max_per_run}\n\
         \n"
    )
}

// ---------------------------------------------------------------------------
// Observers
// ---------------------------------------------------------------------------

/// Whether `r` is the `orchestrator` role of an orchestration tab rooted at
/// `worktree` (the spawn target recorded as `orchestration_cwd`).
fn orchestrator_in(r: &AgentRecord, worktree: &Path) -> bool {
    let want = worktree.to_string_lossy();
    matches!(
        &r.tab_membership,
        Some(TabMembership::Orchestration { role_name, orchestration_cwd, .. })
            if role_name == "orchestrator" && orchestration_cwd.as_deref() == Some(want.as_ref())
    )
}

/// Whether `r` is the role named `role` of an orchestration tab rooted at
/// `worktree` (its `orchestration_cwd`). Generalises [`orchestrator_in`] so a
/// multi-role dispatch can match a specific sibling role (e.g. `reviewer`).
fn role_in(r: &AgentRecord, role: &str, worktree: &Path) -> bool {
    let want = worktree.to_string_lossy();
    matches!(
        &r.tab_membership,
        Some(TabMembership::Orchestration { role_name, orchestration_cwd, .. })
            if role_name == role && orchestration_cwd.as_deref() == Some(want.as_ref())
    )
}

/// Whether `r` is any orchestrator-role pane (used to count dispatched
/// orchestration tabs across issues).
fn is_orchestrator(r: &AgentRecord) -> bool {
    matches!(
        &r.tab_membership,
        Some(TabMembership::Orchestration { role_name, .. }) if role_name == "orchestrator"
    )
}

/// Whether `r` is a non-orchestration single-agent card whose spawn cwd is
/// `worktree`.
fn single_card_in(r: &AgentRecord, worktree: &Path) -> bool {
    let want = worktree.to_string_lossy();
    !matches!(r.tab_membership, Some(TabMembership::Orchestration { .. }))
        && r.cwd.as_deref() == Some(want.as_ref())
}

fn count_orchestrators(daemon: &common::DaemonProc) -> usize {
    daemon
        .agent_records()
        .iter()
        .filter(|r| is_orchestrator(r))
        .count()
}

/// Whether the clone's `git worktree list` still references `worktree`.
fn git_worktree_listed(clone: &Path, worktree: &Path) -> bool {
    let out = common::fixture_git(clone, clone)
        .args(["worktree", "list", "--porcelain"])
        .output();
    match out {
        Ok(o) => {
            let listed = String::from_utf8_lossy(&o.stdout);
            let needle = worktree.to_string_lossy();
            listed.contains(needle.as_ref())
        }
        Err(_) => false,
    }
}

/// Whether the clone has a local branch named `branch`.
fn git_branch_exists(clone: &Path, branch: &str) -> bool {
    let out = common::fixture_git(clone, clone)
        .args(["branch", "--list", branch])
        .output();
    matches!(out, Ok(o) if !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

/// The ceiling on a dispatch outcome that must HAPPEN — a clone, a worktree, an
/// agent registering, its output, a removal. Each is real `git` work and a
/// process start on the daemon's side, so it is [`common::load_scaled`]: a flat
/// 15 s failed `dispatch_007`, `dispatch_008` and `dispatch_009` in PRD #1631's
/// broad e2e run with `io full` at 75-79%, and all three passed alone. Never used
/// for a window in which something must NOT happen; those stay flat.
fn w() -> Duration {
    common::load_scaled(Duration::from_secs(15))
}

fn is_claim_write(call: &[String]) -> bool {
    call.starts_with(&["label".into(), "create".into()])
        || call.starts_with(&["issue".into(), "edit".into()])
        || call.starts_with(&["api".into(), "--method".into()])
}

fn comment_writes(stub: &GhStub, method: &str) -> usize {
    stub.calls()
        .iter()
        .filter(|call| call.starts_with(&["api".into(), "--method".into(), method.into()]))
        .count()
}

fn scheduler_messages(daemon: &common::DaemonProc) -> Vec<String> {
    // DaemonProc's public HOME is under the same sandbox root as its captured
    // stderr; observing this file uses the existing StderrNotifier seam.
    std::fs::read_to_string(daemon.home.parent().unwrap().join("daemon-stderr.log"))
        .expect("read daemon notifications")
        .lines()
        .filter(|line| line.starts_with("[scheduler]"))
        .map(str::to_owned)
        .collect()
}

fn claim_failure_messages(daemon: &common::DaemonProc) -> Vec<String> {
    scheduler_messages(daemon)
        .into_iter()
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("claim") && lower.contains("fail") && lower.contains("#7")
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Scenario: Fire a two-issue dispatch against both an unlabelled repo and a repo whose triage token can use an existing label but cannot provision it.
/// Both issue worktrees and agents appear with their prompts, and both issues receive the claimed label.
/// Each run attempts label provisioning once, before its first issue-label write, including when provisioning fails.
#[spec("scheduler/dispatch/001")]
#[test]
fn dispatch_001_clone_worktree_spawn() {
    let mut provision_counts = Vec::new();
    for deny_provision in [false, true] {
        let stub = GhStub::new();
        let repo = "acme/widgets";
        stub.add_repo(repo, true);
        stub.set_issues(repo, &[7, 8]);
        if deny_provision {
            stub.deny_provision_existing_label(repo);
        }

        let work_td = common::harness_tempdir().expect("workspace tempdir");
        let work = work_td.path().join("ws");
        std::fs::create_dir_all(&work).expect("create workspace root");
        let work_str = work.to_string_lossy().into_owned();

        let toml = dispatch_task(
            "dispatch-task",
            &work_str,
            "ISSUEDISPATCH-{{issue_number}}",
            repo,
            5,
        );
        let path = stub.path_env();
        let ghdir = stub.ghstub_dir();
        let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
        let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

        daemon
            .run_now("dispatch-task")
            .expect("run-now dispatch-task");

        let paths = derive_issue_paths(Path::new(&work_str), "dispatch-task", 7);

        assert!(
            common::wait_for_path(&paths.worktree_dir, w()),
            "firing an issue_dispatch task must create the per-issue worktree at {:?}",
            paths.worktree_dir
        );
        assert!(
            paths.clone_dir.is_dir(),
            "the repo must be cloned to {:?}",
            paths.clone_dir
        );
        assert!(
            common::wait_until(common::load_scaled(Duration::from_secs(5)), || {
                git_branch_exists(&paths.clone_dir, &paths.branch)
            }),
            "the per-issue branch {} must exist in the clone",
            paths.branch
        );

        let agent = daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
            .unwrap_or_else(|| {
                panic!(
                    "dispatch must spawn an orchestrator-role agent into {:?}",
                    paths.worktree_dir
                )
            });
        assert!(
            view::wait_for(
                &daemon.attach_socket,
                agent.pane_id_env.as_deref().expect("the agent's pane id"),
                "ISSUEDISPATCH-7",
                w(),
            ),
            "the substituted per-issue prompt must be delivered to the dispatched orchestrator"
        );
        assert!(
            common::wait_until(w(), || {
                stub.state(repo)["issue_labels"]["7"] == serde_json::json!(["agent-deck/claimed"])
            }),
            "successful dispatch must persist agent-deck/claimed on issue #7; gh calls: {:?}",
            stub.calls()
        );
        let sibling_paths = derive_issue_paths(Path::new(&work_str), "dispatch-task", 8);
        let sibling = daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &sibling_paths.worktree_dir), w())
            .expect("the second issue must spawn an orchestrator too");
        assert!(view::wait_for(
            &daemon.attach_socket,
            sibling.pane_id_env.as_deref().unwrap(),
            "ISSUEDISPATCH-8",
            w(),
        ));
        assert!(
            common::wait_until(w(), || {
                stub.state(repo)["issue_labels"]["8"] == serde_json::json!(["agent-deck/claimed"])
                    && !stub.marker_comments(repo, 8).is_empty()
            }),
            "both issues must be labelled even when provisioning is denied; denied={deny_provision}; calls: {:?}",
            stub.calls()
        );
        let calls = stub.calls();
        let provisions: Vec<_> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.starts_with(&["label".into(), "create".into()]))
            .collect();
        provision_counts.push((deny_provision, provisions.len()));
        assert!(!provisions.is_empty(), "attempt just-in-time provisioning");
        assert!(provisions[0].1.iter().any(|arg| arg == "--force"));
        let claim = calls
            .iter()
            .position(|call| call.starts_with(&["issue".into(), "edit".into()]))
            .expect("issue label write");
        assert!(provisions[0].0 < claim, "provision before adding the label");
        stub.assert_no_unhandled_calls();
    }
    assert_eq!(
        provision_counts,
        vec![(false, 1), (true, 1)],
        "provision the label once per two-issue run, including failed provisioning (denied, calls)"
    );
}

/// Scenario: Fire an `issue_dispatch` task twice (one open issue, no intervening
/// close). After the first fire the issue's worktree and one orchestrator agent
/// exist; after the second fire the already-claimed issue is skipped — the
/// worktree and clone are still present, NO duplicate agent is spawned (the
/// orchestrator count stays at one), and a skip is surfaced through the notifier.
#[spec("scheduler/dispatch/002")]
#[test]
fn dispatch_002_idempotent_skip_existing_worktree() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7]);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    let paths = derive_issue_paths(Path::new(&work_str), "dispatch-task", 7);

    // First fire: dispatch issue 7.
    daemon.run_now("dispatch-task").expect("run-now (first)");
    assert!(
        common::wait_for_path(&paths.worktree_dir, w()),
        "the first fire must create the issue-7 worktree"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
            .is_some(),
        "the first fire must spawn an orchestrator agent for issue 7"
    );
    assert_eq!(
        count_orchestrators(&daemon),
        1,
        "exactly one dispatch after the first fire"
    );

    // Second fire: issue 7 is already claimed (its worktree exists) → skip.
    daemon.run_now("dispatch-task").expect("run-now (second)");

    // No duplicate spawn: the orchestrator count must NOT grow to two.
    let grew = common::wait_until(Duration::from_secs(5), || count_orchestrators(&daemon) > 1);
    assert!(
        !grew,
        "a second fire must NOT re-dispatch the already-claimed issue 7"
    );
    assert!(
        paths.worktree_dir.is_dir(),
        "the existing worktree must be preserved"
    );
    assert!(
        paths.clone_dir.is_dir(),
        "the clone must be preserved (no re-clone error)"
    );

    let surfaced = common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
        daemon.stderr_contains("issue-7")
            || daemon.stderr_contains("issue 7")
            || daemon.stderr_contains("skip")
            || daemon.stderr_contains("Skip")
    });
    assert!(
        surfaced,
        "the skip of the already-claimed issue must be logged/surfaced"
    );
}

/// Scenario: With two open issues where `gh pr list` reports an open PR whose
/// head is `agent/issue-7` (and none for issue 8), fire the task. Issue 8
/// dispatches (its worktree exists, an orchestrator agent runs there), proving
/// the flow ran; issue 7 is skipped via the secondary idempotency signal — no
/// `issue-7` worktree is created and no agent is spawned for it.
#[spec("scheduler/dispatch/003")]
#[test]
fn dispatch_003_skip_open_pr() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7, 8]);
    stub.set_open_pr(repo, 7);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    daemon
        .run_now("dispatch-task")
        .expect("run-now dispatch-task");

    let p7 = derive_issue_paths(Path::new(&work_str), "dispatch-task", 7);
    let p8 = derive_issue_paths(Path::new(&work_str), "dispatch-task", 8);

    // Issue 8 (no PR) dispatches — proves the flow ran end to end.
    assert!(
        common::wait_for_path(&p8.worktree_dir, w()),
        "issue 8 (no open PR) must be dispatched: its worktree must exist"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &p8.worktree_dir), w())
            .is_some(),
        "issue 8 must spawn an orchestrator agent"
    );

    // Issue 7 is skipped by the open-PR secondary signal.
    assert!(
        !p7.worktree_dir.exists(),
        "issue 7 (open PR on agent/issue-7) must be skipped — no worktree created"
    );
    assert_eq!(
        count_orchestrators(&daemon),
        1,
        "only issue 8 should dispatch; issue 7 (open PR) is skipped"
    );
}

/// Scenario: Fire two `issue_dispatch` tasks whose resolved commands are bare
/// Codex — one from a cloned orchestration role and one from the single-agent
/// registry default. Both prompts must reach their panes, and PATH recorders must
/// prove both dispatch paths launched through the Codex Wrapper strategy.
#[spec("scheduler/dispatch/004")]
#[test]
fn dispatch_004_orchestration_vs_single_agent() {
    let stub = GhStub::new();
    let orch_repo = "acme/orch";
    let plain_repo = "acme/plain";
    let orch_fixture = stub.repo_dir(orch_repo);
    std::fs::create_dir_all(&orch_fixture).expect("create Codex orchestration fixture");
    init_remote_with_orch_toml(
        &orch_fixture.join("remote"),
        Some(
            "[[orchestrations]]\nname = \"dispatch-orch\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"codex\"\nstart = true\n",
        ),
    );
    stub.add_repo(plain_repo, false);
    stub.set_issues(orch_repo, &[11]);
    stub.set_issues(plain_repo, &[22]);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    // A single-agent dispatch resolves its command from `default_command`.
    let cfg_td = common::harness_tempdir().expect("config tempdir");
    let cfg = cfg_td.path().join("config.toml");
    std::fs::write(&cfg, "default_command = \"codex\"\n").expect("write config.toml");
    let launch_record = cfg_td.path().join("dispatch-launch.log");
    let wrapper_stub = stub.bindir.join("dot-agent-deck");
    let codex_stub = stub.bindir.join("codex");
    std::fs::write(
        &wrapper_stub,
        "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$CODEX_DISPATCH_RECORD\"\nexec cat\n",
    )
    .expect("write dispatch wrapper recorder");
    std::fs::write(
        &codex_stub,
        "#!/bin/sh\nprintf 'BARE codex %s\\n' \"$*\" >> \"$CODEX_DISPATCH_RECORD\"\nexec cat\n",
    )
    .expect("write dispatch bare recorder");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for executable in [&wrapper_stub, &codex_stub] {
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
                .expect("chmod dispatch recorder");
        }
    }

    let mut toml = dispatch_task(
        "task-orch",
        &work_str,
        "ORCHDISP-{{issue_number}}",
        orch_repo,
        5,
    );
    toml.push_str(&dispatch_task(
        "task-plain",
        &work_str,
        "PLAINDISP-{{issue_number}}",
        plain_repo,
        5,
    ));

    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let cfg_str = cfg.to_string_lossy().into_owned();
    let record_str = launch_record.to_string_lossy().into_owned();
    let wrapper_stub_str = wrapper_stub.to_string_lossy().into_owned();
    let env: Vec<(&str, &str)> = vec![
        ("PATH", path.as_str()),
        ("GHSTUB_DIR", ghdir.as_str()),
        ("DOT_AGENT_DECK_CONFIG", cfg_str.as_str()),
        ("CODEX_DISPATCH_RECORD", record_str.as_str()),
        // The rewrite names the co-located build by absolute path now, so a
        // fake `dot-agent-deck` on PATH is no longer what the wrapper
        // resolves to. Point the rewrite at the recorder explicitly.
        ("DOT_AGENT_DECK_WRAP_BIN", wrapper_stub_str.as_str()),
    ];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    daemon.run_now("task-orch").expect("run-now task-orch");
    daemon.run_now("task-plain").expect("run-now task-plain");

    let orch_wt = derive_issue_paths(Path::new(&work_str), "task-orch", 11).worktree_dir;
    let plain_wt = derive_issue_paths(Path::new(&work_str), "task-plain", 22).worktree_dir;

    // Orchestration clone → orchestration tab + prompt to the orchestrator role.
    assert!(
        common::wait_for_path(&orch_wt, w()),
        "orchestration issue worktree must exist"
    );
    let orch_agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &orch_wt), w())
        .expect("orchestration clone must open an orchestrator tab in the worktree");
    assert!(
        view::wait_for(
            &daemon.attach_socket,
            orch_agent
                .pane_id_env
                .as_deref()
                .expect("the agent's pane id"),
            "ORCHDISP-11",
            w(),
        ),
        "the prompt must reach the orchestrator role of the orchestration dispatch"
    );

    // Plain clone → single-agent card + prompt delivered.
    assert!(
        common::wait_for_path(&plain_wt, w()),
        "single-agent issue worktree must exist"
    );
    let plain_agent = daemon
        .wait_for_agent_where(|r| single_card_in(r, &plain_wt), w())
        .expect("plain clone must open a single-agent card in the worktree");
    assert!(
        view::wait_for(
            &daemon.attach_socket,
            plain_agent
                .pane_id_env
                .as_deref()
                .expect("the agent's pane id"),
            "PLAINDISP-22",
            w(),
        ),
        "the prompt must reach the single-agent card of the plain dispatch"
    );
    assert!(
        common::wait_for_file_substr_count(&launch_record, "WRAPPED", 2, w()),
        "both issue-dispatch Codex paths must reach a launch recorder"
    );
    // PRD #20 §4.2.1: the daemon's startup scoped-trust step runs `codex
    // app-server` to read Codex's own hook hashes, which this PATH recorder also
    // logs. It is a metadata probe, not an agent launch, so it is filtered out.
    let launches = common::recorded_agent_launches(&launch_record);
    assert_eq!(
        launches,
        vec![
            "WRAPPED wrap --agent codex -- codex",
            "WRAPPED wrap --agent codex -- codex",
        ],
        "issue-dispatch single-agent and role spawns must both cross the Wrapper strategy; observed:\n{}",
        launches.join("\n")
    );
}

/// Scenario: With a stub `gh` returning five open issues but `max_per_run = 2`,
/// fire the task. Only the first two issues (in returned order) get worktrees
/// and orchestrator spawns; the remaining three are left untouched (no
/// worktrees), and exactly two orchestrator agents exist.
#[spec("scheduler/dispatch/005")]
#[test]
fn dispatch_005_respects_max_per_run() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[1, 2, 3, 4, 5]);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        2,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    daemon
        .run_now("dispatch-task")
        .expect("run-now dispatch-task");

    let pd = |n: u64| derive_issue_paths(Path::new(&work_str), "dispatch-task", n);

    // The first two issues are dispatched — the worktree AND the orchestrator
    // agent, waited for separately, because they are two steps of the dispatch
    // and only the first has landed when the directory appears. The count
    // below is a snapshot with no wait of its own, so without this it reads
    // the registry mid-spawn and sees one orchestrator where two are coming:
    // measured on a loaded box, `left: 1, right: 2`. Every sibling test in
    // this file already pairs the two waits this way (`dispatch_003`,
    // `dispatch_007`, `dispatch_008`); this one was the exception.
    for n in [1u64, 2] {
        let paths = pd(n);
        assert!(
            common::wait_for_path(&paths.worktree_dir, w()),
            "issue {n} must be dispatched"
        );
        assert!(
            daemon
                .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
                .is_some(),
            "issue {n} must spawn an orchestrator agent before the cap is counted"
        );
    }

    // The remaining three are left untouched.
    for n in [3u64, 4, 5] {
        assert!(
            !pd(n).worktree_dir.exists(),
            "issue {n} is past max_per_run=2 and must NOT be dispatched"
        );
    }

    assert_eq!(
        count_orchestrators(&daemon),
        2,
        "max_per_run=2 must cap the run at two per-issue spawns"
    );
}

/// Scenario: Dispatch an issue and close its tab through StopAgent on the orchestrator.
/// Its worktree disappears from disk and git's registry while the clone remains, and close makes no GitHub release calls or changes to the claim state.
#[spec("scheduler/dispatch/006")]
#[test]
fn dispatch_006_close_removes_worktree_preserves_clone() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7]);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    daemon
        .run_now("dispatch-task")
        .expect("run-now dispatch-task");

    let paths = derive_issue_paths(Path::new(&work_str), "dispatch-task", 7);
    assert!(
        common::wait_for_path(&paths.worktree_dir, w()),
        "the issue worktree must exist before close"
    );
    let agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
        .expect("dispatch must spawn the orchestrator before close");

    assert!(daemon.wait_for_stderr_contains("dispatched issue #7", w()));
    let calls_before_close = stub.calls();
    let claim_before_close = stub.state(repo);

    // Close the dispatched tab.
    daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: agent.id.clone(),
        })
        .expect("StopAgent over the attach socket");

    // Cleanup removes the worktree (disk + git registry) but preserves the clone.
    let removed = common::wait_until(w(), || {
        !paths.worktree_dir.exists() && !git_worktree_listed(&paths.clone_dir, &paths.worktree_dir)
    });
    assert!(
        removed,
        "closing the dispatched tab must remove the worktree from disk and `git worktree list`"
    );
    assert!(
        paths.clone_dir.is_dir(),
        "the clone directory must be preserved after worktree cleanup"
    );
    assert!(
        !common::wait_until(Duration::from_secs(2), || stub.calls()
            != calls_before_close),
        "closing a dispatched tab must make no GitHub calls, including label removal, comment DELETE, or another release: {:?}",
        stub.calls()
    );
    assert_eq!(
        stub.state(repo),
        claim_before_close,
        "close preserves the GitHub claim"
    );
    stub.assert_no_unhandled_calls();
}

/// Scenario: With two open issues where `gh pr list --head agent/issue-11` errors
/// (a simulated per-issue GitHub failure) while issue 10 is healthy, fire the
/// task. Issue 10 still dispatches (its worktree exists, one orchestrator runs),
/// issue 11 does not (no worktree), and the failure is surfaced through the
/// notifier rather than swallowed — proving one issue's failure does not abort
/// the rest of the run.
#[spec("scheduler/dispatch/007")]
#[test]
fn dispatch_007_one_issue_fails_others_dispatch() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[10, 11]);
    stub.fail_pr(repo, 11);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    daemon
        .run_now("dispatch-task")
        .expect("run-now dispatch-task");

    let p10 = derive_issue_paths(Path::new(&work_str), "dispatch-task", 10);
    let p11 = derive_issue_paths(Path::new(&work_str), "dispatch-task", 11);

    // The healthy issue still dispatches.
    assert!(
        common::wait_for_path(&p10.worktree_dir, w()),
        "issue 10 must still dispatch despite issue 11 failing"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &p10.worktree_dir), w())
            .is_some(),
        "issue 10 must spawn an orchestrator agent"
    );

    // The failing issue does not.
    assert!(
        !p11.worktree_dir.exists(),
        "the failing issue 11 must NOT produce a worktree"
    );
    assert_eq!(
        count_orchestrators(&daemon),
        1,
        "only the healthy issue 10 should dispatch"
    );

    // The failure is surfaced, not swallowed.
    let surfaced = common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
        daemon.stderr_contains("issue-11")
            || daemon.stderr_contains("issue 11")
            || daemon.stderr_contains("#11")
    });
    assert!(
        surfaced,
        "the per-issue failure for issue 11 must be surfaced through the notifier"
    );
}

/// Scenario: Copy a genuine claim from a separate issue into issue 7 as a comment authored by another GitHub account, then dispatch issue 7.
/// The deck leaves the forged record untouched and POSTs its own authenticated record.
/// Close issue 7's agent and fire again: the worktree and agent return, and only the deck-owned record is PATCHed.
#[spec("scheduler/dispatch/008")]
#[test]
fn dispatch_008_refire_after_close_redispatches() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_authenticated_user(54321, "claim-owner");
    stub.set_issues(repo, &[6]);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    // Learn the exact host field from this daemon's genuine record, so the
    // forgery remains byte-for-byte valid when its digest representation changes.
    daemon.run_now("dispatch-task").expect("write source claim");
    assert!(common::wait_until(w(), || {
        stub.marker_comments(repo, 6).len() == 1
    }));
    assert!(daemon.wait_for_stderr_contains("dispatched issue #6", w()));
    let mut forged = stub.marker_comments(repo, 6).remove(0);
    assert_eq!(forged["user"]["id"], 54321);
    assert!(forged["body"].as_str().unwrap().starts_with(CLAIM_MARKER));
    assert!(forged["body"].as_str().unwrap().contains("dispatch-task"));
    forged["user"] = serde_json::json!({"id": 99999, "login": "forger"});
    let forged_id = stub.seed_comment(repo, 7, forged);
    let forged_record = stub.marker_comments(repo, 7).remove(0);
    stub.set_issues(repo, &[7]);
    let own_records = || {
        stub.marker_comments(repo, 7)
            .into_iter()
            .filter(|comment| comment["user"]["id"] == 54321)
            .collect::<Vec<_>>()
    };

    let paths = derive_issue_paths(Path::new(&work_str), "dispatch-task", 7);

    // First fire: dispatch issue 7.
    daemon.run_now("dispatch-task").expect("run-now (first)");
    assert!(
        common::wait_for_path(&paths.worktree_dir, w()),
        "the first fire must create the issue-7 worktree"
    );
    let agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
        .expect("the first fire must spawn the orchestrator for issue 7");

    assert!(common::wait_until(w(), || {
        comment_writes(&stub, "POST") + comment_writes(&stub, "PATCH") > 1
    }));
    let forged_endpoint = format!("repos/{repo}/issues/comments/{forged_id}");
    assert!(
        !stub.calls().iter().any(|call| {
            call.starts_with(&["api".into(), "--method".into(), "PATCH".into()])
                && call.get(3) == Some(&forged_endpoint)
        }),
        "a byte-copy claim authored by another user must never be PATCHed; gh calls: {:?}",
        stub.calls()
    );
    assert!(
        common::wait_until(w(), || own_records().len() == 1),
        "first dispatch must POST its own authenticated marker comment beside the forgery; gh calls: {:?}",
        stub.calls()
    );
    let first_record = own_records().remove(0);
    assert_eq!(stub.marker_comments(repo, 7).len(), 2);
    assert!(
        first_record["body"]
            .as_str()
            .unwrap()
            .contains("dispatch-task")
    );
    assert_eq!(
        comment_writes(&stub, "POST"),
        2,
        "source claim and issue-7 claim each POST their own record once"
    );
    assert_eq!(comment_writes(&stub, "PATCH"), 0);

    // Close the dispatched tab. Cleanup removes the worktree but `git worktree
    // remove --force` LEAVES the branch `agent/issue-7` behind.
    daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: agent.id.clone(),
        })
        .expect("StopAgent over the attach socket");
    assert!(
        common::wait_until(w(), || !paths.worktree_dir.exists()),
        "closing the dispatched tab must remove the worktree (precondition for the re-fire)"
    );
    // Precondition for B1: the branch survives the close, so a naive
    // `worktree add -b agent/issue-7` on the next fire would collide.
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(5)), || {
            git_branch_exists(&paths.clone_dir, &paths.branch)
        }),
        "the close preserves branch {} (the leftover that makes the re-fire trip)",
        paths.branch
    );

    // Second fire: issue still open, no PR, worktree gone → must re-dispatch.
    daemon.run_now("dispatch-task").expect("run-now (second)");

    // B1 fixed: `create_worktree` tolerates the pre-existing `agent/issue-7`
    // branch (reusing it rather than `-b`-creating it), so the second fire
    // re-creates the worktree and re-dispatches instead of surfacing an
    // `IssueDispatchFailed`.
    assert!(
        common::wait_for_path(&paths.worktree_dir, w()),
        "re-firing must re-create the issue-7 worktree (B1: worktree-add must tolerate the existing branch)"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
            .is_some(),
        "re-firing must spawn the orchestrator again for the reclaimed issue 7"
    );
    assert!(
        common::wait_until(w(), || {
            let records = own_records();
            comment_writes(&stub, "PATCH") == 1
                && records.len() == 1
                && records[0]["updated_at"] != first_record["updated_at"]
        }),
        "re-claim must PATCH and persist the existing comment edit; gh calls: {:?}",
        stub.calls()
    );
    let records = own_records();
    assert_eq!(
        records.len(),
        1,
        "re-claim leaves exactly one deck-owned marker comment"
    );
    assert_eq!(
        records[0]["id"], first_record["id"],
        "edit the original record"
    );
    assert!(
        records[0]["body"]
            .as_str()
            .unwrap()
            .contains("dispatch-task")
    );
    assert_eq!(
        comment_writes(&stub, "POST"),
        2,
        "re-claim must not POST again"
    );
    assert_eq!(
        stub.marker_comments(repo, 7)
            .into_iter()
            .find(|comment| comment["id"] == forged_id)
            .unwrap(),
        forged_record,
        "the foreign record remains unchanged across dispatch and re-claim"
    );
    let own_endpoint = format!("repos/{repo}/issues/comments/{}", first_record["id"]);
    assert!(
        stub.calls()
            .iter()
            .filter(|call| { call.starts_with(&["api".into(), "--method".into(), "PATCH".into()]) })
            .all(|call| call.get(3) == Some(&own_endpoint))
    );
    stub.assert_no_unhandled_calls();
    // The re-dispatch must not surface a per-issue failure for issue 7.
    let failed = common::wait_until(Duration::from_secs(3), || {
        daemon.stderr_contains("failed:") || daemon.stderr_contains("already exists")
    });
    assert!(
        !failed,
        "re-dispatch must NOT surface an IssueDispatchFailed for issue 7"
    );
}

/// Scenario: Dispatch one open issue whose cloned repo carries a TWO-role
/// orchestration (orchestrator + reviewer), so the issue worktree hosts two role
/// panes sharing one `orchestration_cwd`. Closing ONE role pane must leave the
/// worktree on disk (a sibling role is still live in it); only closing the LAST
/// role pane removes the worktree — and the clone is preserved (S1: refcount the
/// worktree, removing it only when the last rooted agent closes).
#[spec("scheduler/dispatch/009")]
#[test]
fn dispatch_009_multirole_orchestration_cleanup_refcount() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo_multirole(repo);
    stub.set_issues(repo, &[7]);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    daemon
        .run_now("dispatch-task")
        .expect("run-now dispatch-task");

    let paths = derive_issue_paths(Path::new(&work_str), "dispatch-task", 7);
    let wt = paths.worktree_dir.clone();
    assert!(
        common::wait_for_path(&wt, w()),
        "dispatch must create the issue worktree"
    );

    // Both role panes spawn into the SAME issue worktree.
    let orchestrator = daemon
        .wait_for_agent_where(|r| role_in(r, "orchestrator", &wt), w())
        .expect("the orchestrator role must spawn into the issue worktree");
    let reviewer = daemon
        .wait_for_agent_where(|r| role_in(r, "reviewer", &wt), w())
        .expect("the reviewer role must spawn into the same issue worktree");

    // Close ONE role (the reviewer). The worktree must SURVIVE — the
    // orchestrator role is still live in it.
    daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: reviewer.id.clone(),
        })
        .expect("StopAgent reviewer over the attach socket");

    // S1 fixed: the shared worktree is refcounted, so the first role-pane close
    // does not run `git worktree remove --force` while the orchestrator role is
    // still live in it. Give the async cleanup time to (wrongly) fire, then
    // assert the worktree is still present on disk and in `git worktree list`.
    let nuked_early = common::wait_until(Duration::from_secs(5), || {
        !wt.exists() || !git_worktree_listed(&paths.clone_dir, &wt)
    });
    assert!(
        !nuked_early,
        "closing ONE role of a multi-role orchestration must NOT remove the shared worktree (S1: refcount — remove only on the LAST close)"
    );

    // Close the LAST role (the orchestrator). Now the worktree must be removed,
    // while the clone is preserved.
    daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: orchestrator.id.clone(),
        })
        .expect("StopAgent orchestrator over the attach socket");
    let removed = common::wait_until(w(), || {
        !wt.exists() && !git_worktree_listed(&paths.clone_dir, &wt)
    });
    assert!(
        removed,
        "closing the LAST role must remove the shared worktree from disk and `git worktree list`"
    );
    assert!(
        paths.clone_dir.is_dir(),
        "the clone directory must be preserved after worktree cleanup"
    );
}

/// Scenario: Dispatch an issue, arm a failing open-PR check, and fire again with its worktree still present.
/// The issue is skipped without a failure or duplicate spawn, its clone and worktree remain, and this nothing-to-dispatch fire makes no label-provisioning or claim-write calls.
#[spec("scheduler/dispatch/012")]
#[test]
fn dispatch_012_worktree_present_skips_without_pr_check() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7]);

    let work_td = common::harness_tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    let toml = dispatch_task(
        "dispatch-task",
        &work_str,
        "ISSUEDISPATCH-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let env: Vec<(&str, &str)> = vec![("PATH", path.as_str()), ("GHSTUB_DIR", ghdir.as_str())];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    let paths = derive_issue_paths(Path::new(&work_str), "dispatch-task", 7);

    // First fire (no fail marker yet): issue 7 dispatches normally, so its
    // worktree and one orchestrator exist — the precondition for the short-circuit.
    daemon.run_now("dispatch-task").expect("run-now (first)");
    assert!(
        common::wait_for_path(&paths.worktree_dir, w()),
        "the first fire must create the issue-7 worktree"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
            .is_some(),
        "the first fire must spawn an orchestrator agent for issue 7"
    );
    assert_eq!(
        count_orchestrators(&daemon),
        1,
        "exactly one dispatch after the first fire"
    );

    // Arm the hazard: from now on `gh pr list --head agent/issue-7` exits
    // non-zero. With the OLD code this transient error — reached because the PR
    // check ran unconditionally — turned the worktree-present SKIP into a
    // spurious IssueDispatchFailed. The fix short-circuits before the check.
    stub.fail_pr(repo, 7);

    assert!(daemon.wait_for_stderr_contains("dispatched issue #7", w()));
    let writes_before_refire = stub
        .calls()
        .iter()
        .filter(|call| is_claim_write(call))
        .count();

    // Second fire: the worktree already exists → primary signal → SKIP without
    // ever calling `issue_has_open_pr`.
    daemon.run_now("dispatch-task").expect("run-now (second)");

    // No duplicate spawn / re-creation: the orchestrator count must NOT grow.
    let grew = common::wait_until(Duration::from_secs(5), || count_orchestrators(&daemon) > 1);
    assert!(
        !grew,
        "a worktree-present second fire must NOT re-dispatch or re-spawn issue 7"
    );

    // The present worktree must short-circuit to an IssueDispatchSkipped notice.
    // (With the OLD code the PR-check error fires first and no skip is surfaced,
    // so this wait would time out — making the test RED against the regression.)
    let skipped = common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
        daemon.stderr_contains("already-claimed issue #7")
    });
    assert!(
        skipped,
        "the present worktree must short-circuit to a SKIP (IssueDispatchSkipped) for issue 7"
    );

    // ...and because the PR check is never reached, the simulated `gh pr list`
    // error must NEVER surface as a per-issue failure for issue 7.
    assert!(
        !daemon.stderr_contains("issue #7 of acme/widgets failed"),
        "the worktree-present skip must short-circuit before issue_has_open_pr, so the simulated gh pr list error must NOT surface as IssueDispatchFailed"
    );

    // The existing worktree and clone are preserved (no re-creation, no re-clone).
    assert!(
        paths.worktree_dir.is_dir(),
        "the existing issue-7 worktree must be preserved"
    );
    assert!(
        paths.clone_dir.is_dir(),
        "the clone must be preserved (no re-clone)"
    );
    assert_eq!(
        stub.calls()
            .iter()
            .filter(|call| is_claim_write(call))
            .count(),
        writes_before_refire,
        "a fire dispatching nothing must not provision labels or write claims; calls: {:?}",
        stub.calls()
    );
    stub.assert_no_unhandled_calls();
}

/// Scenario: Fire issue dispatch against a repo whose token rejects every GitHub write.
/// The agent still spawns and receives its prompt, and exactly one claim-failure notification identifies both the label and record outcomes separately from dispatch success or failure.
#[spec("scheduler/dispatch/029")]
#[test]
fn dispatch_029_read_only_claim_failure_keeps_agent_running() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7]);
    stub.read_only(repo);
    let initial_state = stub.state(repo);
    let work = common::harness_tempdir().expect("workspace");
    let task = dispatch_task(
        "dispatch-task",
        &work.path().to_string_lossy(),
        "READONLY-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let daemon = common::spawn_daemon_serve_with_env(
        Some(&task),
        "0",
        &[("PATH", &path), ("GHSTUB_DIR", &ghdir)],
    );
    daemon
        .run_now("dispatch-task")
        .expect("fire read-only dispatch");
    let paths = derive_issue_paths(work.path(), "dispatch-task", 7);
    let agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), w())
        .expect("read-only GitHub must not prevent the agent spawn");
    assert!(
        view::wait_for(
            &daemon.attach_socket,
            agent.pane_id_env.as_deref().unwrap(),
            "READONLY-7",
            w()
        ),
        "read-only dispatch delivers its prompt"
    );
    assert!(
        common::wait_until(w(), || !claim_failure_messages(&daemon).is_empty()),
        "read-only claim must raise IssueClaimFailed separately from dispatch notifications; notifications: {:?}; gh calls: {:?}",
        scheduler_messages(&daemon),
        stub.calls()
    );
    assert!(daemon.wait_for_stderr_contains("dispatched issue #7", w()));
    assert!(
        !common::wait_until(Duration::from_secs(2), || claim_failure_messages(&daemon)
            .len()
            > 1),
        "one failed claim raises exactly one notification"
    );
    let messages = claim_failure_messages(&daemon);
    assert_eq!(messages.len(), 1);
    let message = messages[0].to_lowercase();
    assert!(
        message.contains("label") && message.contains("record"),
        "IssueClaimFailed names both outcomes: {messages:?}"
    );
    assert!(message.contains("dispatch-task") && message.contains(repo));
    assert!(
        !daemon.stderr_contains("issue #7 of acme/widgets failed:"),
        "claim failure is not IssueDispatchFailed"
    );
    assert_eq!(
        stub.state(repo),
        initial_state,
        "permission denial mutates no GitHub state"
    );
    assert_eq!(
        comment_writes(&stub, "POST"),
        1,
        "record creation is attempted even if label provisioning fails"
    );
    stub.assert_no_unhandled_calls();
}

/// Scenario: Fire issue dispatch against an orchestration whose start role names a missing executable.
/// The issue worktree is prepared but spawning fails visibly, no agent runs there, and GitHub receives no label provisioning, issue edit or comment write.
#[spec("scheduler/dispatch/030")]
#[test]
fn dispatch_030_spawn_failure_writes_no_claim() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, false);
    let remote = stub.repo_dir(repo).join("remote");
    std::fs::write(
        remote.join(".dot-agent-deck.toml"),
        ORCH_TOML.replace(
            "command = \"cat\"",
            "command = \"/missing-dispatch-agent-421\"",
        ),
    )
    .expect("seed missing agent command");
    run_git(&remote, &["add", ".dot-agent-deck.toml"]);
    run_git(&remote, &["commit", "-q", "-m", "unavailable agent"]);
    stub.set_issues(repo, &[7]);
    let work = common::harness_tempdir().expect("workspace");
    let task = dispatch_task(
        "dispatch-task",
        &work.path().to_string_lossy(),
        "SPAWNFAIL-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let daemon = common::spawn_daemon_serve_with_env(
        Some(&task),
        "0",
        &[("PATH", &path), ("GHSTUB_DIR", &ghdir)],
    );
    daemon.run_now("dispatch-task").expect("fire failing spawn");
    assert!(
        daemon.wait_for_stderr_contains("issue #7 of acme/widgets failed:", w()),
        "spawn failure must be surfaced: {:?}",
        scheduler_messages(&daemon)
    );
    let paths = derive_issue_paths(work.path(), "dispatch-task", 7);
    assert!(
        paths.worktree_dir.is_dir(),
        "spawn was attempted after preparing the issue worktree"
    );
    assert!(
        !daemon
            .agent_records()
            .iter()
            .any(|r| orchestrator_in(r, &paths.worktree_dir)),
        "no agent spawned for the failed issue"
    );
    assert!(
        !common::wait_until(Duration::from_secs(2), || stub
            .calls()
            .iter()
            .any(|call| is_claim_write(call))),
        "spawn failure must make no label create, issue edit or comment write: {:?}",
        stub.calls()
    );
    stub.assert_no_unhandled_calls();
}

/// Scenario: Dispatch two issues while the first issue's GitHub label write hangs and the daemon has a 1500 ms claim-call timeout.
/// Both agents must spawn, with the second dispatched within 20 seconds of the hanging call, and the healthy issue gets its GitHub claim.
/// Exactly one claim-failure notice names the first issue's timed-out label step, separately from successful dispatch.
#[spec("scheduler/dispatch/031")]
#[test]
fn dispatch_031_hanging_claim_does_not_hold_queue() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7, 8]);
    stub.hang_issue_label(repo, 7);
    let work = common::harness_tempdir().expect("workspace");
    let task = dispatch_task(
        "dispatch-task",
        &work.path().to_string_lossy(),
        "TIMEOUT-{{issue_number}}",
        repo,
        5,
    );
    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let daemon = common::spawn_daemon_serve_with_env(
        Some(&task),
        "0",
        &[
            ("PATH", &path),
            ("GHSTUB_DIR", &ghdir),
            ("DOT_AGENT_DECK_ISSUE_CLAIM_TIMEOUT_MS", "1500"),
        ],
    );
    daemon.run_now("dispatch-task").expect("fire two issues");
    let first = derive_issue_paths(work.path(), "dispatch-task", 7);
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &first.worktree_dir), w())
            .is_some(),
        "the first issue spawns before its hanging claim"
    );
    assert!(common::wait_until(w(), || {
        stub.calls().iter().any(|call| {
            call.starts_with(&["issue".into(), "edit".into()])
                && call.last().map(String::as_str) == Some("7")
        })
    }));
    let started = std::time::Instant::now();
    // This is a responsiveness contract, not a load-scaled eventual outcome:
    // 20 s leaves room for git/spawn while detecting the old 600 s gh bound.
    let ceiling = Duration::from_secs(20);
    let second = derive_issue_paths(work.path(), "dispatch-task", 8);
    let sibling = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &second.worktree_dir), ceiling)
        .unwrap_or_else(|| {
            panic!(
                "a hanging issue-7 claim must not hold issue 8 beyond 20 s; elapsed={:?}; notifications={:?}; gh calls={:?}",
                started.elapsed(),
                scheduler_messages(&daemon),
                stub.calls()
            )
        });
    assert!(started.elapsed() < ceiling);
    assert_eq!(count_orchestrators(&daemon), 2);
    assert!(view::wait_for(
        &daemon.attach_socket,
        sibling.pane_id_env.as_deref().unwrap(),
        "TIMEOUT-8",
        w(),
    ));
    assert!(common::wait_until(w(), || {
        stub.state(repo)["issue_labels"]["8"] == serde_json::json!(["agent-deck/claimed"])
            && stub.marker_comments(repo, 8).len() == 1
            && !claim_failure_messages(&daemon).is_empty()
    }));
    assert!(daemon.wait_for_stderr_contains("dispatched issue #8", w()));
    assert!(
        !common::wait_until(Duration::from_secs(2), || {
            claim_failure_messages(&daemon).len() > 1
        }),
        "a timed-out claim raises one notification, not one per step"
    );
    let messages = claim_failure_messages(&daemon);
    assert_eq!(messages.len(), 1);
    let message = messages[0].to_lowercase();
    assert!(
        message.contains("label") && (message.contains("timed out") || message.contains("timeout")),
        "IssueClaimFailed must name the timed-out label step: {messages:?}"
    );
    assert!(
        message.contains("record") && message.contains(repo) && message.contains("dispatch-task")
    );
    assert!(
        !daemon.stderr_contains("issue #7 of acme/widgets failed:"),
        "claim timeout must not become a dispatch failure"
    );
    stub.assert_no_unhandled_calls();
}
