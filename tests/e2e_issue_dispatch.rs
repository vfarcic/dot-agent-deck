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

    /// Like [`add_repo`] but the committed `.dot-agent-deck.toml` defines a
    /// TWO-role orchestration (orchestrator + reviewer, both `cat`), so a
    /// dispatched worktree opens an orchestration tab with two role panes that
    /// share the SAME issue worktree as their `orchestration_cwd`.
    fn add_repo_multirole(&self, repo: &str) {
        let rd = self.repo_dir(repo);
        std::fs::create_dir_all(&rd).expect("create repo fixture dir");
        init_remote_with_orch_toml(&rd.join("remote"), Some(MULTIROLE_ORCH_TOML));
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

    let work_td = tempfile::tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    // A single-agent dispatch resolves its command from `default_command`.
    let cfg_td = tempfile::tempdir().expect("config tempdir");
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
    let env: Vec<(&str, &str)> = vec![
        ("PATH", path.as_str()),
        ("GHSTUB_DIR", ghdir.as_str()),
        ("DOT_AGENT_DECK_CONFIG", cfg_str.as_str()),
        ("CODEX_DISPATCH_RECORD", record_str.as_str()),
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
    assert!(
        common::wait_for_file_substr_count(&launch_record, "codex", 2, W),
        "both issue-dispatch Codex paths must reach a launch recorder"
    );
    let launches = std::fs::read_to_string(&launch_record).expect("read dispatch launch record");
    assert_eq!(
        launches.lines().collect::<Vec<_>>(),
        vec![
            "WRAPPED wrap --agent codex -- codex",
            "WRAPPED wrap --agent codex -- codex",
        ],
        "issue-dispatch single-agent and role spawns must both cross the Wrapper strategy; observed:\n{launches}"
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

/// Scenario: Dispatch one open issue (no open PR) so its worktree and branch
/// `agent/issue-7` are created, then CLOSE the dispatched agent via `StopAgent`
/// (removing the worktree but LEAVING the branch), then fire the SAME task again
/// while `gh` still reports the issue open with no PR. The reclaimed issue must
/// re-dispatch — its worktree is re-created and an orchestrator spawns again —
/// with no per-issue failure surfaced (B1: worktree-add must tolerate the
/// pre-existing branch left behind by the close).
#[spec("scheduler/dispatch/008")]
#[test]
fn dispatch_008_refire_after_close_redispatches() {
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
    let agent = daemon
        .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), W)
        .expect("the first fire must spawn the orchestrator for issue 7");

    // Close the dispatched tab. Cleanup removes the worktree but `git worktree
    // remove --force` LEAVES the branch `agent/issue-7` behind.
    daemon
        .send_attach_request(&AttachRequest::StopAgent {
            id: agent.id.clone(),
        })
        .expect("StopAgent over the attach socket");
    assert!(
        common::wait_until(W, || !paths.worktree_dir.exists()),
        "closing the dispatched tab must remove the worktree (precondition for the re-fire)"
    );
    // Precondition for B1: the branch survives the close, so a naive
    // `worktree add -b agent/issue-7` on the next fire would collide.
    assert!(
        common::wait_until(Duration::from_secs(5), || git_branch_exists(
            &paths.clone_dir,
            &paths.branch
        )),
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
        common::wait_for_path(&paths.worktree_dir, W),
        "re-firing must re-create the issue-7 worktree (B1: worktree-add must tolerate the existing branch)"
    );
    assert!(
        daemon
            .wait_for_agent_where(|r| orchestrator_in(r, &paths.worktree_dir), W)
            .is_some(),
        "re-firing must spawn the orchestrator again for the reclaimed issue 7"
    );
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
    let wt = paths.worktree_dir.clone();
    assert!(
        common::wait_for_path(&wt, W),
        "dispatch must create the issue worktree"
    );

    // Both role panes spawn into the SAME issue worktree.
    let orchestrator = daemon
        .wait_for_agent_where(|r| role_in(r, "orchestrator", &wt), W)
        .expect("the orchestrator role must spawn into the issue worktree");
    let reviewer = daemon
        .wait_for_agent_where(|r| role_in(r, "reviewer", &wt), W)
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
    let removed = common::wait_until(W, || {
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

/// Scenario: Dispatch one open issue so its worktree exists, then arm the stub so
/// `gh pr list --head agent/issue-7` ERRORS, and fire again. Because a present
/// worktree is the primary idempotency signal, the second fire must short-circuit
/// to a SKIP and return BEFORE consulting the open-PR check — so the simulated
/// `gh` error never runs and never surfaces as a per-issue failure. Assert the
/// issue is reported SKIPPED (not FAILED), with no duplicate spawn and the
/// worktree/clone preserved (regression guard for the PRD #120 / Greptile P1
/// short-circuit fix, commit 212bc73).
#[spec("scheduler/dispatch/012")]
#[test]
fn dispatch_012_worktree_present_skips_without_pr_check() {
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

    // First fire (no fail marker yet): issue 7 dispatches normally, so its
    // worktree and one orchestrator exist — the precondition for the short-circuit.
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

    // Arm the hazard: from now on `gh pr list --head agent/issue-7` exits
    // non-zero. With the OLD code this transient error — reached because the PR
    // check ran unconditionally — turned the worktree-present SKIP into a
    // spurious IssueDispatchFailed. The fix short-circuits before the check.
    stub.fail_pr(repo, 7);

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
    let skipped = common::wait_until(Duration::from_secs(10), || {
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
}
