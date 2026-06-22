//! Fire-time GitHub issue-dispatch flow (PRD #120, M2.1–M2.4 + M3.2 + M1.3).
//!
//! This is the impure, daemon-side counterpart to the pure helpers in
//! [`crate::issue_dispatch`]. On each fire of an `issue_dispatch` scheduled task
//! the daemon composes those helpers with #127's spawn primitive
//! ([`crate::spawn::spawn`]) and the `gh` / `git` binaries on `PATH`:
//!
//!   1. **M2.1** — provision the repo clone under the task's `working_dir`:
//!      clone-if-missing (`gh repo clone`) / fetch + fast-forward-pull-if-present
//!      (`git -C <clone> fetch` then `git -C <clone> pull --ff-only`).
//!   2. enumerate the repo's open issues (`gh issue list`), capping at
//!      `max_per_run` **in code** on the returned order — the issue list may
//!      ignore `--limit`.
//!   3. **M2.2** — for each issue, decide dispatch-vs-skip from the two
//!      idempotency signals (per-issue worktree already on disk; an open PR whose
//!      head is `agent/issue-<n>`) via [`crate::issue_dispatch::dispatch_decision`].
//!   4. **M2.2 / M2.3** — on dispatch, create the per-issue worktree
//!      (`git -C <clone> worktree add <worktree> -b agent/issue-<n>`) and
//!      [`spawn`] one agent into it, delivering the substituted prompt. The spawn
//!      primitive already branches on the worktree's `.dot-agent-deck.toml`
//!      (orchestration tab vs single-agent card) — reused, not duplicated.
//!   5. **M2.4** — record each spawned pane → worktree in a daemon-side
//!      [`WorktreeRegistry`] so closing the tab later removes the worktree (while
//!      PRESERVING the clone). See [`record_worktree`] / [`take_worktree`] /
//!      [`remove_worktree`].
//!   6. **M3.2** — every issue runs inside its own error boundary: a failing
//!      issue (clone/worktree/`gh` error — e.g. the test stub's simulated
//!      `pr list` failure) is surfaced through the notifier and the run CONTINUES
//!      with the remaining issues. One issue never aborts the rest.
//!   7. **M1.3** — per-issue success / skip / failure events are surfaced through
//!      #127's existing [`Notifier`] seam (no parallel notification system).
//!
//! All GitHub/git access goes through the `gh` / `git` binaries resolved from
//! `PATH`, inheriting the daemon's environment — that is exactly what lets the
//! L2 tests isolate everything offline behind a stub `gh` on `PATH` plus a local
//! fixture remote.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::agent_pty::{AgentPtyRegistry, AgentRecord, TabMembership};
use crate::config::IssueDispatchConfig;
use crate::event::BroadcastMsg;
use crate::issue_dispatch::{
    DispatchDecision, derive_issue_paths, dispatch_decision, issue_list_argv,
    pr_list_for_issue_argv, substitute_issue_number,
};
use crate::scheduler::{Notifier, NotifyEvent};
use crate::spawn::{SpawnRequest, spawn};

// ---------------------------------------------------------------------------
// M2.4 — daemon-side worktree registry (close → cleanup plumbing)
// ---------------------------------------------------------------------------

/// Daemon-owned, in-memory map: per-issue worktree dir → the clone that owns it
/// (preserved on cleanup). Shared between the fire-time dispatch flow (records
/// the worktree the moment it is created — BEFORE the spawn's prompt-delivery
/// wait returns) and the `StopAgent` handler (removes it on close).
///
/// Keyed by the **worktree path**, not the spawned agent id, on purpose: the
/// spawn primitive only returns the registry id AFTER its readiness/delivery
/// wait, so a tab closed promptly after the agent appears would race a
/// per-agent-id record. The closing agent is instead matched to its worktree via
/// its [`AgentRecord`] (orchestration cwd / single-agent cwd) — available the
/// instant the agent is registered. Wiped on daemon restart; a post-restart
/// close finds no entry and leaves the worktree in place (reclaimed by the
/// worktree-exists idempotency signal on the next fire).
pub type WorktreeRegistry = Arc<Mutex<HashMap<PathBuf, PathBuf>>>;

/// Construct an empty [`WorktreeRegistry`].
pub fn new_worktree_registry() -> WorktreeRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Record a freshly-created per-issue worktree (→ its owning clone) for
/// tab-close cleanup. Idempotent: a re-recorded worktree just refreshes the
/// clone mapping.
pub fn record_worktree(worktrees: &WorktreeRegistry, worktree_dir: &Path, clone_dir: &Path) {
    worktrees
        .lock()
        .expect("worktree registry poisoned")
        .insert(worktree_dir.to_path_buf(), clone_dir.to_path_buf());
}

/// The per-issue worktree a closing agent was dispatched into, derived from its
/// [`AgentRecord`]: the orchestration cwd for an orchestration tab, else the
/// single-agent card's cwd. `None` for an agent that carries neither.
pub fn worktree_of_record(record: &AgentRecord) -> Option<PathBuf> {
    match &record.tab_membership {
        Some(TabMembership::Orchestration {
            orchestration_cwd, ..
        }) => orchestration_cwd.clone().map(PathBuf::from),
        _ => record.cwd.clone().map(PathBuf::from),
    }
}

/// If `worktree_dir` is a dispatched issue worktree, drop its registry entry and
/// return the owning clone dir; `None` otherwise (an ordinary agent's cwd, or an
/// already-cleaned sibling-role close). The first close of a multi-role tab
/// takes the entry; later sibling closes find nothing and no-op.
pub fn take_worktree(worktrees: &WorktreeRegistry, worktree_dir: &Path) -> Option<PathBuf> {
    worktrees
        .lock()
        .expect("worktree registry poisoned")
        .remove(worktree_dir)
}

/// Remove a dispatched worktree from its clone (`git -C <clone> worktree remove
/// <worktree> --force`), PRESERVING the clone. Best-effort: a non-zero exit
/// (already removed, locked) or a spawn error is logged, not fatal — the tab is
/// already gone.
pub async fn remove_worktree(worktree_dir: &Path, clone_dir: &Path) {
    let res = run_status(
        "git",
        &[
            "-C",
            &clone_dir.to_string_lossy(),
            "worktree",
            "remove",
            &worktree_dir.to_string_lossy(),
            "--force",
        ],
    )
    .await;
    match res {
        Ok(()) => tracing::info!(
            worktree = %worktree_dir.display(),
            "issue-dispatch: removed worktree on tab close (clone preserved)"
        ),
        Err(e) => tracing::warn!(
            worktree = %worktree_dir.display(),
            error = %e,
            "issue-dispatch: worktree cleanup on close failed"
        ),
    }
}

// ---------------------------------------------------------------------------
// Fire-time dispatch flow
// ---------------------------------------------------------------------------

/// Run the full issue-dispatch flow for one fire of an `issue_dispatch` task.
///
/// `default_command` is the resolved single-agent command (from the global
/// `default_command`, or the task's own command) — used only for clones with no
/// orchestration config; orchestration clones ignore it (the role commands win).
///
/// Never panics; the repo-level steps abort only this repo's fire (one repo per
/// task, no fan-out) and every issue runs inside its own error boundary.
#[allow(clippy::too_many_arguments)]
pub async fn run_issue_dispatch(
    task_name: &str,
    working_dir: &str,
    prompt_template: &str,
    cfg: &IssueDispatchConfig,
    default_command: Option<String>,
    registry: &AgentPtyRegistry,
    worktrees: &WorktreeRegistry,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
) {
    let workspace = Path::new(working_dir);
    // `<working_dir>/<task name>` — identical to `derive_issue_paths(..).clone_dir`.
    let clone_dir = workspace.join(task_name);

    // M2.1 — provision the repo clone (clone-if-missing / fetch+ff-pull-if-present).
    if let Err(message) = provision_repo(workspace, &clone_dir, &cfg.repo).await {
        notifier.notify(NotifyEvent::IssueDispatchRepoError {
            task: task_name.to_string(),
            repo: cfg.repo.clone(),
            message,
        });
        return;
    }

    // Enumerate open issues. The `--limit` in the argv is advisory; cap in code.
    let issues = match list_open_issues(cfg).await {
        Ok(v) => v,
        Err(message) => {
            notifier.notify(NotifyEvent::IssueDispatchRepoError {
                task: task_name.to_string(),
                repo: cfg.repo.clone(),
                message,
            });
            return;
        }
    };

    for issue in issues.into_iter().take(cfg.max_per_run) {
        // M3.2 — per-issue error boundary: one failure never aborts the rest.
        if let Err(message) = dispatch_one_issue(
            task_name,
            working_dir,
            prompt_template,
            cfg,
            default_command.as_deref(),
            issue,
            &clone_dir,
            registry,
            worktrees,
            notifier,
            event_tx,
        )
        .await
        {
            notifier.notify(NotifyEvent::IssueDispatchFailed {
                task: task_name.to_string(),
                repo: cfg.repo.clone(),
                issue,
                message,
            });
        }
    }
}

/// Process one candidate issue. `Ok(())` means it was dispatched OR skipped (a
/// skip is surfaced here, not treated as an error); `Err` is a per-issue failure
/// for the caller to surface through the notifier (M3.2).
#[allow(clippy::too_many_arguments)]
async fn dispatch_one_issue(
    task_name: &str,
    working_dir: &str,
    prompt_template: &str,
    cfg: &IssueDispatchConfig,
    default_command: Option<&str>,
    issue: u64,
    clone_dir: &Path,
    registry: &AgentPtyRegistry,
    worktrees: &WorktreeRegistry,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
) -> Result<(), String> {
    let paths = derive_issue_paths(Path::new(working_dir), task_name, issue);

    // M2.2 — idempotency BEFORE any work. Primary: the worktree already exists.
    // Secondary: an open PR whose head is `agent/issue-<n>`. A `gh` failure on
    // the PR check is a per-issue error (e.g. the stub's simulated API error).
    let worktree_exists = paths.worktree_dir.exists();
    let open_pr = issue_has_open_pr(&cfg.repo, issue).await?;

    if dispatch_decision(worktree_exists, open_pr) == DispatchDecision::Skip {
        notifier.notify(NotifyEvent::IssueDispatchSkipped {
            task: task_name.to_string(),
            repo: cfg.repo.clone(),
            issue,
            branch: paths.branch.clone(),
        });
        return Ok(());
    }

    // M2.2 — create the per-issue worktree on `agent/issue-<n>`.
    create_worktree(clone_dir, &paths.worktree_dir, &paths.branch).await?;

    // M2.4 — record the worktree for tab-close cleanup NOW, before the spawn's
    // prompt-delivery wait. `spawn` registers the agent (visible to a `StopAgent`
    // from a fast client) well before it returns, so recording after the spawn
    // would race a prompt close. The close watcher matches the agent to this
    // worktree by its record's cwd, not by an agent id we don't have yet.
    record_worktree(worktrees, &paths.worktree_dir, clone_dir);

    // M2.3 — spawn one agent into the worktree, delivering the substituted
    // prompt. `spawn` branches on the worktree's `.dot-agent-deck.toml`.
    let req = SpawnRequest {
        task_name: task_name.to_string(),
        working_dir: paths.worktree_dir.to_string_lossy().into_owned(),
        command: default_command.map(str::to_string),
        prompt: substitute_issue_number(prompt_template, issue),
    };
    if let Err(e) = spawn(req, registry, notifier, event_tx).await {
        // The spawn failed after the worktree was created/recorded: no agent
        // will ever close to trigger cleanup, so drop the registry entry here.
        // The worktree dir itself is left on disk — the next fire's
        // worktree-exists idempotency signal reclaims the issue.
        take_worktree(worktrees, &paths.worktree_dir);
        return Err(e.to_string());
    }

    // M1.3 — surface the per-issue dispatch success.
    notifier.notify(NotifyEvent::IssueDispatched {
        task: task_name.to_string(),
        repo: cfg.repo.clone(),
        issue,
    });
    Ok(())
}

/// M2.1: clone the repo if its dir is missing, else refresh the existing clone
/// (fetch + fast-forward pull). `gh` / `git` are resolved from `PATH` and inherit
/// the daemon's environment.
async fn provision_repo(workspace: &Path, clone_dir: &Path, repo: &str) -> Result<(), String> {
    if clone_dir.is_dir() {
        let clone = clone_dir.to_string_lossy();
        run_status("git", &["-C", &clone, "fetch"]).await?;
        run_status("git", &["-C", &clone, "pull", "--ff-only"]).await?;
        return Ok(());
    }
    std::fs::create_dir_all(workspace)
        .map_err(|e| format!("failed to create workspace {}: {e}", workspace.display()))?;
    run_status("gh", &["repo", "clone", repo, &clone_dir.to_string_lossy()]).await
}

/// Enumerate the repo's open issue numbers in returned order.
async fn list_open_issues(cfg: &IssueDispatchConfig) -> Result<Vec<u64>, String> {
    let argv = issue_list_argv(
        &cfg.repo,
        cfg.max_per_run,
        cfg.label.as_deref(),
        cfg.query.as_deref(),
    );
    let stdout = run_capture("gh", &argv).await?;
    parse_issue_numbers(&stdout)
}

/// The secondary idempotency signal: whether an open PR's head is
/// `agent/issue-<n>`. A non-empty `gh pr list` JSON array means yes.
async fn issue_has_open_pr(repo: &str, issue: u64) -> Result<bool, String> {
    let argv = pr_list_for_issue_argv(repo, issue);
    let stdout = run_capture("gh", &argv).await?;
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("failed to parse `gh pr list` JSON: {e}"))?;
    Ok(value.as_array().is_some_and(|a| !a.is_empty()))
}

/// M2.2: `git -C <clone> worktree add <worktree> -b agent/issue-<n>`. The
/// `.worktrees` parent is created first so the add never trips on a missing dir.
async fn create_worktree(
    clone_dir: &Path,
    worktree_dir: &Path,
    branch: &str,
) -> Result<(), String> {
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create worktree parent {}: {e}", parent.display()))?;
    }
    run_status(
        "git",
        &[
            "-C",
            &clone_dir.to_string_lossy(),
            "worktree",
            "add",
            &worktree_dir.to_string_lossy(),
            "-b",
            branch,
        ],
    )
    .await
}

/// Parse a `gh issue list --json number` array into issue numbers, in order.
/// Entries missing a numeric `number` are skipped rather than failing the whole
/// parse.
fn parse_issue_numbers(json: &str) -> Result<Vec<u64>, String> {
    let value: serde_json::Value = serde_json::from_str(json.trim())
        .map_err(|e| format!("failed to parse `gh issue list` JSON: {e}"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| "`gh issue list` did not return a JSON array".to_string())?;
    Ok(arr
        .iter()
        .filter_map(|item| item.get("number").and_then(serde_json::Value::as_u64))
        .collect())
}

/// Run a subprocess that must exit zero; on failure return a message carrying
/// the program, args, exit status, and any stderr.
async fn run_status(program: &str, args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to run `{program}`: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`{program} {}` failed ({}): {}",
        args.join(" "),
        output.status,
        stderr.trim()
    ))
}

/// Run a subprocess that must exit zero and return its captured stdout. Accepts
/// `String` args (the `gh` argv helpers produce `Vec<String>`).
async fn run_capture(program: &str, args: &[String]) -> Result<String, String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("failed to run `{program}`: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            output.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_issue_numbers_reads_number_field_in_order() {
        let json = r#"[{"number":7},{"number":8},{"number":3}]"#;
        assert_eq!(parse_issue_numbers(json).unwrap(), vec![7, 8, 3]);
    }

    #[test]
    fn parse_issue_numbers_empty_array() {
        assert_eq!(parse_issue_numbers("[]\n").unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn parse_issue_numbers_rejects_non_array() {
        assert!(parse_issue_numbers("{}").is_err());
        assert!(parse_issue_numbers("not json").is_err());
    }

    #[test]
    fn record_then_take_worktree_returns_clone_once() {
        let reg = new_worktree_registry();
        let wt7 = PathBuf::from("/ws/task/.worktrees/issue-7");
        let wt8 = PathBuf::from("/ws/task/.worktrees/issue-8");
        let clone = PathBuf::from("/ws/task");
        record_worktree(&reg, &wt7, &clone);
        record_worktree(&reg, &wt8, &clone);

        // The first close of issue-7's tab returns its clone and drops the entry;
        // a second close (a sibling role) finds nothing. issue-8 is untouched.
        assert_eq!(take_worktree(&reg, &wt7), Some(clone.clone()));
        assert_eq!(take_worktree(&reg, &wt7), None);
        assert_eq!(take_worktree(&reg, &wt8), Some(clone));
    }

    #[test]
    fn take_worktree_none_for_unrecorded_path() {
        let reg = new_worktree_registry();
        assert_eq!(take_worktree(&reg, Path::new("/not/dispatched")), None);
    }

    /// Minimal [`AgentRecord`] for the cwd-derivation test (the struct has no
    /// `Default`); only `cwd` + `tab_membership` matter to `worktree_of_record`.
    fn record(cwd: Option<&str>, membership: Option<TabMembership>) -> AgentRecord {
        AgentRecord {
            id: "a1".into(),
            pane_id_env: None,
            display_name: None,
            cwd: cwd.map(str::to_string),
            tab_membership: membership,
            agent_type: None,
            rows: 24,
            cols: 80,
        }
    }

    #[test]
    fn worktree_of_record_prefers_orchestration_cwd_else_cwd() {
        // Orchestration tab → the orchestration cwd is the worktree (its own cwd
        // is ignored).
        let orch = record(
            Some("/ignored"),
            Some(TabMembership::Orchestration {
                name: "x".into(),
                role_index: 0,
                role_name: "orchestrator".into(),
                is_start_role: true,
                orchestration_cwd: Some("/ws/task/.worktrees/issue-7".into()),
                display_title: None,
            }),
        );
        assert_eq!(
            worktree_of_record(&orch),
            Some(PathBuf::from("/ws/task/.worktrees/issue-7"))
        );

        // Single-agent card → its cwd is the worktree.
        let single = record(Some("/ws/task/.worktrees/issue-9"), None);
        assert_eq!(
            worktree_of_record(&single),
            Some(PathBuf::from("/ws/task/.worktrees/issue-9"))
        );

        // Neither → None.
        assert_eq!(worktree_of_record(&record(None, None)), None);
    }
}
