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
//! RED today: firing an `issue_dispatch` task does NOT run the GitHub-dispatch
//! flow — nothing is cloned, no worktree is created, and no per-issue agent is
//! spawned into a worktree (the current callback just attempts a single bogus
//! spawn in the workspace root). Each test fails on its first observable —
//! clone/worktree/per-issue-spawn absent — exactly the flow the later
//! flow-implementer task wires up.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::issue_dispatch::derive_issue_paths;
use spec::spec;

// ---------------------------------------------------------------------------
// Stub `gh` + local fixture-remote harness (the TESTABILITY SEAM)
// ---------------------------------------------------------------------------

/// A synthetic `gh` that branches on argv, reading all of its canned data from
/// files under `$GHSTUB_DIR`, keyed by the `--repo owner/name` (with `/` → `_`):
///   - `issue list` → prints `<key>/issues.json`;
///   - `pr list --head agent/issue-<n>` → exits 1 if `<key>/fail-pr-<n>` exists
///     (a simulated per-issue GitHub error), else prints `<key>/pr-<n>.json` if
///     present, else `[]`;
///   - `repo clone <repo> <dest>` → `git clone <key>/remote <dest>` (the local
///     fixture remote becomes the clone's `origin`).
const GH_STUB_SCRIPT: &str = r#"#!/bin/sh
# Synthetic `gh` for PRD #120 issue_dispatch L2 tests — fully offline.
group="$1"
sub="$2"
shift 2 2>/dev/null || true

if [ "$group" = "repo" ] && [ "$sub" = "clone" ]; then
    repo="$1"
    dest="$2"
    key=$(printf '%s' "$repo" | tr '/' '_')
    exec git clone --quiet "$GHSTUB_DIR/$key/remote" "$dest"
fi

repo=""
head=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --repo) shift; repo="$1" ;;
        --head) shift; head="$1" ;;
        *) ;;
    esac
    shift
done
key=$(printf '%s' "$repo" | tr '/' '_')

if [ "$group" = "issue" ] && [ "$sub" = "list" ]; then
    cat "$GHSTUB_DIR/$key/issues.json"
    exit 0
fi

if [ "$group" = "pr" ] && [ "$sub" = "list" ]; then
    n=${head##*-}
    if [ -f "$GHSTUB_DIR/$key/fail-pr-$n" ]; then
        echo "gh: simulated API error for issue $n (agent/issue-$n)" 1>&2
        exit 1
    fi
    if [ -f "$GHSTUB_DIR/$key/pr-$n.json" ]; then
        cat "$GHSTUB_DIR/$key/pr-$n.json"
    else
        printf '[]\n'
    fi
    exit 0
fi

echo "gh stub: unhandled invocation: $group $sub $*" 1>&2
exit 1
"#;

/// A committed `.dot-agent-deck.toml` whose single-role orchestration runs `cat`
/// (which echoes the delivered prompt) — mirrors `scheduler/spawn/002`'s
/// orchestration fixture so a dispatched worktree opens an orchestration tab.
const ORCH_TOML: &str = "[[orchestrations]]\nname = \"dispatch-orch\"\n\n\
     [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n";

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
        let scratch = tempfile::tempdir().expect("gh stub scratch tempdir");
        let base = scratch.path().to_path_buf();
        let dir = base.join("ghstub");
        let bindir = base.join("bin");
        std::fs::create_dir_all(&dir).expect("create ghstub data dir");
        std::fs::create_dir_all(&bindir).expect("create ghstub bin dir");
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
/// plus `.dot-agent-deck.toml` when `with_orchestration`). Commit identity is
/// pinned inline so the repo does not depend on the host's global git config.
fn init_remote(remote: &Path, with_orchestration: bool) {
    std::fs::create_dir_all(remote).expect("create remote dir");
    run_git(remote, &["-c", "init.defaultBranch=main", "init", "-q"]);
    std::fs::write(remote.join("README.md"), "issue-dispatch fixture\n").expect("write README");
    if with_orchestration {
        std::fs::write(remote.join(".dot-agent-deck.toml"), ORCH_TOML)
            .expect("write fixture .dot-agent-deck.toml");
    }
    run_git(remote, &["add", "-A"]);
    run_git(
        remote,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(clone)
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(clone)
        .args(["branch", "--list", branch])
        .output();
    matches!(out, Ok(o) if !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

const W: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Scenario: With a stub `gh` returning one open issue (7) and a fixture remote
/// carrying an orchestration config, fire an `issue_dispatch` task via run-now.
/// Assert the repo is cloned to `<working_dir>/<name>`, a per-issue worktree
/// appears at `<clone>/.worktrees/issue-7` on branch `agent/issue-7`, and an
/// orchestrator-role agent is spawned into that worktree with the substituted
/// per-issue prompt delivered (echoed by `cat`).
#[spec("scheduler/dispatch/001")]
#[test]
fn dispatch_001_clone_worktree_spawn() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7]);

    let work_td = tempfile::tempdir().expect("workspace tempdir");
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
        common::wait_for_path(&paths.worktree_dir, W),
        "firing an issue_dispatch task must create the per-issue worktree at {:?}",
        paths.worktree_dir
    );
    assert!(
        paths.clone_dir.is_dir(),
        "the repo must be cloned to {:?}",
        paths.clone_dir
    );
    assert!(
        common::wait_until(Duration::from_secs(5), || git_branch_exists(
            &paths.clone_dir,
            &paths.branch
        )),
        "the per-issue branch {} must exist in the clone",
        paths.branch
    );

    let agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), W)
        .unwrap_or_else(|| {
            panic!(
                "dispatch must spawn an orchestrator-role agent into {:?}",
                paths.worktree_dir
            )
        });
    assert!(
        daemon.attach_and_wait_for_output(&agent.id, "ISSUEDISPATCH-7", W),
        "the substituted per-issue prompt must be delivered to the dispatched orchestrator"
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

    let work_td = tempfile::tempdir().expect("workspace tempdir");
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
        common::wait_for_path(&paths.worktree_dir, W),
        "the first fire must create the issue-7 worktree"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), W)
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

    let surfaced = common::wait_until(Duration::from_secs(10), || {
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

    let work_td = tempfile::tempdir().expect("workspace tempdir");
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
        common::wait_for_path(&p8.worktree_dir, W),
        "issue 8 (no open PR) must be dispatched: its worktree must exist"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &p8.worktree_dir), W)
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

/// Scenario: Fire two `issue_dispatch` tasks — one whose cloned repo carries an
/// orchestration `.dot-agent-deck.toml`, one whose clone has none (single-agent,
/// `default_command = cat`). Assert the orchestration clone opens an orchestrator
/// tab in its worktree with the substituted prompt delivered, while the plain
/// clone opens a non-orchestration single-agent card in its worktree with the
/// substituted prompt delivered.
#[spec("scheduler/dispatch/004")]
#[test]
fn dispatch_004_orchestration_vs_single_agent() {
    let stub = GhStub::new();
    let orch_repo = "acme/orch";
    let plain_repo = "acme/plain";
    stub.add_repo(orch_repo, true);
    stub.add_repo(plain_repo, false);
    stub.set_issues(orch_repo, &[11]);
    stub.set_issues(plain_repo, &[22]);

    let work_td = tempfile::tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    // A single-agent dispatch resolves its command from `default_command`.
    let cfg_td = tempfile::tempdir().expect("config tempdir");
    let cfg = cfg_td.path().join("config.toml");
    std::fs::write(&cfg, "default_command = \"cat\"\n").expect("write config.toml");

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
    let env: Vec<(&str, &str)> = vec![
        ("PATH", path.as_str()),
        ("GHSTUB_DIR", ghdir.as_str()),
        ("DOT_AGENT_DECK_CONFIG", cfg_str.as_str()),
    ];
    let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &env);

    daemon.run_now("task-orch").expect("run-now task-orch");
    daemon.run_now("task-plain").expect("run-now task-plain");

    let orch_wt = derive_issue_paths(Path::new(&work_str), "task-orch", 11).worktree_dir;
    let plain_wt = derive_issue_paths(Path::new(&work_str), "task-plain", 22).worktree_dir;

    // Orchestration clone → orchestration tab + prompt to the orchestrator role.
    assert!(
        common::wait_for_path(&orch_wt, W),
        "orchestration issue worktree must exist"
    );
    let orch_agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &orch_wt), W)
        .expect("orchestration clone must open an orchestrator tab in the worktree");
    assert!(
        daemon.attach_and_wait_for_output(&orch_agent.id, "ORCHDISP-11", W),
        "the prompt must reach the orchestrator role of the orchestration dispatch"
    );

    // Plain clone → single-agent card + prompt delivered.
    assert!(
        common::wait_for_path(&plain_wt, W),
        "single-agent issue worktree must exist"
    );
    let plain_agent = daemon
        .wait_for_agent_where(|r| single_card_in(r, &plain_wt), W)
        .expect("plain clone must open a single-agent card in the worktree");
    assert!(
        daemon.attach_and_wait_for_output(&plain_agent.id, "PLAINDISP-22", W),
        "the prompt must reach the single-agent card of the plain dispatch"
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

    let work_td = tempfile::tempdir().expect("workspace tempdir");
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

    // The first two issues are dispatched.
    assert!(
        common::wait_for_path(&pd(1).worktree_dir, W),
        "issue 1 must be dispatched"
    );
    assert!(
        common::wait_for_path(&pd(2).worktree_dir, W),
        "issue 2 must be dispatched"
    );

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

/// Scenario: After dispatching an issue, close the dispatched tab (StopAgent on
/// its orchestrator) and assert the daemon-side close→cleanup plumbing removes
/// the per-issue worktree — it disappears from disk and from `git worktree list`
/// — while the clone directory is preserved.
#[spec("scheduler/dispatch/006")]
#[test]
fn dispatch_006_close_removes_worktree_preserves_clone() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_repo(repo, true);
    stub.set_issues(repo, &[7]);

    let work_td = tempfile::tempdir().expect("workspace tempdir");
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
        common::wait_for_path(&paths.worktree_dir, W),
        "the issue worktree must exist before close"
    );
    let agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), W)
        .expect("dispatch must spawn the orchestrator before close");

    // Close the dispatched tab.
    daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: agent.id.clone(),
        })
        .expect("StopAgent over the attach socket");

    // Cleanup removes the worktree (disk + git registry) but preserves the clone.
    let removed = common::wait_until(W, || {
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

    let work_td = tempfile::tempdir().expect("workspace tempdir");
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
        common::wait_for_path(&p10.worktree_dir, W),
        "issue 10 must still dispatch despite issue 11 failing"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &p10.worktree_dir), W)
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
    let surfaced = common::wait_until(Duration::from_secs(10), || {
        daemon.stderr_contains("issue-11")
            || daemon.stderr_contains("issue 11")
            || daemon.stderr_contains("#11")
    });
    assert!(
        surfaced,
        "the per-issue failure for issue 11 must be surfaced through the notifier"
    );
}
