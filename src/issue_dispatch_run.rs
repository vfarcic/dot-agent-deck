//! Fire-time GitHub issue-dispatch flow (PRD #120, M2.1–M2.4 + M3.2 + M1.3).
//!
//! This is the impure, daemon-side counterpart to the pure helpers in
//! [`crate::issue_dispatch`]. On each fire of an `issue_dispatch` scheduled task
//! the daemon composes those helpers with #127's spawn primitive
//! ([`crate::spawn::spawn`]) and the `gh` / `git` binaries on `PATH`:
//!
//!   1. **M2.1** — provision the repo clone under the task's `working_dir`:
//!      clone-if-missing (`gh repo clone`) / fetch + fast-forward-pull-if-present
//!      (`git -C <clone> fetch` then `git -C <clone> pull --ff-only`). An existing
//!      clone is verified to be the right repo by its `origin` before being
//!      touched (L3, fail-closed), and a refresh failure on it is non-fatal —
//!      the run continues with the refs already on disk (S3).
//!   2. enumerate the repo's open issues (`gh issue list`), capping at
//!      `max_per_run` **in code** on the returned order — the issue list may
//!      ignore `--limit`.
//!   3. **M2.2** — for each issue, decide dispatch-vs-skip from the two
//!      idempotency signals (per-issue worktree already on disk; an open PR whose
//!      head is `agent/issue-<n>`) via [`crate::issue_dispatch::dispatch_decision`].
//!   4. **M2.2 / M2.3** — on dispatch, create the per-issue worktree on
//!      `agent/issue-<n>` (creating the branch with `-b`, or attaching a branch
//!      left behind by an earlier closed-without-PR run — B1) and [`spawn`] one
//!      agent into it, delivering the substituted prompt. The spawn primitive
//!      already branches on the worktree's `.dot-agent-deck.toml` (orchestration
//!      tab vs single-agent card) — reused, not duplicated.
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
use std::time::Duration;

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
use crate::worktree_owner::Creator;

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
pub type WorktreeRegistry = Arc<Mutex<HashMap<PathBuf, WorktreeEntry>>>;

/// What the close handler needs to clean up one recorded worktree: the clone
/// that owns it (always preserved) and which removal policy applies.
///
/// The policy travels WITH the entry because the tab-close handler
/// (`daemon_protocol.rs`) is shared by both producers and cannot otherwise tell
/// them apart — it sees only a path. Inferring provenance from the path shape
/// (`<clone>/.worktrees/issue-<n>` vs. the `<repo>-dispatch-<slug>` sibling)
/// would silently apply the wrong policy the moment either layout changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// The clone that owns the worktree. Preserved by removal.
    pub clone_dir: PathBuf,
    /// Removal policy — see [`RemovalPolicy`].
    pub policy: RemovalPolicy,
}

/// Whether a recorded worktree may be removed while it still holds
/// uncommitted work.
///
/// The two producers want opposite things, and both are right for their case:
///
/// * [`RemovalPolicy::Force`] — PRD #120 issue-dispatch. The worktree lives
///   inside a daemon-owned `gh repo clone`, never a human checkout, and the
///   reuse-the-vacated-slot model *depends* on the directory actually going
///   away: `dispatch_decision` treats a present worktree as "issue already
///   claimed", so a worktree left behind skips that issue on every later fire,
///   permanently. Forcing is what keeps the slot reclaimable.
/// * [`RemovalPolicy::KeepIfDirty`] — PRD #220 dispatch. The name is chosen by
///   an LLM and the tree is a sibling of the user's own checkout, so Ctrl+W
///   reads as "close this view", not "destroy uncommitted work". A leaked
///   worktree costs disk; a force-removed one costs work, and that asymmetry
///   decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalPolicy {
    /// Remove unconditionally (`--force`), discarding uncommitted changes.
    Force,
    /// Refuse to remove a worktree with uncommitted changes; leave it in place
    /// and log so the user can recover the work.
    KeepIfDirty,
}

/// Construct an empty [`WorktreeRegistry`].
pub fn new_worktree_registry() -> WorktreeRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Record a freshly-created worktree (→ its owning clone + removal policy) for
/// tab-close cleanup. Idempotent: a re-recorded worktree just refreshes the
/// entry.
pub fn record_worktree(
    worktrees: &WorktreeRegistry,
    worktree_dir: &Path,
    clone_dir: &Path,
    policy: RemovalPolicy,
) {
    worktrees.lock().unwrap_or_else(|e| e.into_inner()).insert(
        worktree_dir.to_path_buf(),
        WorktreeEntry {
            clone_dir: clone_dir.to_path_buf(),
            policy,
        },
    );
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

/// If `worktree_dir` is a dispatched worktree, drop its registry entry and
/// return it (owning clone + removal policy); `None` otherwise (an ordinary
/// agent's cwd, or an entry already taken). The close watcher only calls this
/// once it has confirmed (via [`worktree_still_in_use`]) that the LAST agent
/// rooted in the worktree has closed, so for a multi-role orchestration the
/// entry survives every earlier sibling close and is taken exactly once, on the
/// final close.
pub fn take_worktree(worktrees: &WorktreeRegistry, worktree_dir: &Path) -> Option<WorktreeEntry> {
    worktrees
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(worktree_dir)
}

/// S1: whether any live agent in `records` is still rooted in `worktree_dir` —
/// its orchestration cwd (shared by EVERY role pane of a multi-role
/// orchestration) or a single-agent card's cwd. The close watcher calls this
/// AFTER `close_agent` has dropped the closing agent, so an empty result means
/// the just-closed agent was the LAST one in the worktree and it is safe to
/// remove. While a sibling role is still live the shared worktree must survive.
pub fn worktree_still_in_use(records: &[AgentRecord], worktree_dir: &Path) -> bool {
    agents_rooted_in_worktree(records, worktree_dir) > 0
}

/// How many live agents in `records` are rooted in `worktree_dir` — the counting
/// form of [`worktree_still_in_use`], which is defined in terms of it so the two
/// can never disagree about what "rooted in" means.
///
/// Issue #575: the dispatch spawn-failure rollback reports the number back to the
/// caller, because "2 agents are still running in it" tells the user what to close
/// and a bare "still in use" does not.
pub fn agents_rooted_in_worktree(records: &[AgentRecord], worktree_dir: &Path) -> usize {
    records
        .iter()
        .filter(|r| worktree_of_record(r).as_deref() == Some(worktree_dir))
        .count()
}

/// Does `worktree_dir` hold uncommitted work? The ONE `git status --porcelain`
/// this feature asks, so the removal path and issue #717's close-confirmation
/// preview cannot disagree about what "dirty" means.
///
/// `Err` is "the question could not be answered" (no `git`, the directory is
/// gone, a locked index), never "clean" — both callers treat it as a reason to
/// KEEP, because the fail-safe direction for a deletion gate is to decline.
pub async fn worktree_is_dirty(worktree_dir: &Path) -> Result<bool, String> {
    let worktree = worktree_dir.to_string_lossy();
    let output = run_capture_args("git", &["-C", &worktree, "status", "--porcelain"]).await?;
    Ok(!output.trim().is_empty())
}

/// Issue #717: the close-confirmation dialog's advance warning that a
/// dispatched worktree is about to be KEPT rather than removed.
///
/// Used by both halves of the report — the close dialog's PREDICTION
/// ([`kept_worktree_preview`], made while the agent is still alive) and the
/// daemon's post-cleanup FACT ([`remove_worktree`], measured once the agent is
/// reaped). Carries the path because recovering the work means going to it —
/// the whole point of the report.
///
/// `confirmed_dirty` separates the two ways a tree ends up kept, which read
/// differently to a human:
///
/// * `true` — [`worktree_is_dirty`] answered yes. It can be stated flatly.
/// * `false` — the tree is kept, but not because uncommitted work was measured:
///   the probe failed, or blew the preview's deadline, or the `git worktree
///   remove` itself failed. `KeepIfDirty` keeps the tree in all of those cases,
///   so the report is still true; it just has to be phrased conditionally.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeptWorktree {
    /// Absolute path of the worktree that will be left on disk.
    pub path: String,
    /// Whether `git status --porcelain` actually reported uncommitted work, as
    /// opposed to the probe being inconclusive. See the type docs.
    pub confirmed_dirty: bool,
}

/// Issue #717: would closing the panes in `pane_ids` leave a dispatched
/// worktree behind, and if so, which one?
///
/// Answers the question the close-confirmation dialog needs BEFORE the
/// keystroke, from the only process that can: the registry that knows the
/// removal policy and the filesystem that holds the tree both live here, and in
/// remote mode neither is on the client's machine at all.
///
/// Three properties are deliberate:
///
/// * **It PEEKS at the registry, never [`take_worktree`].** This runs while the
///   user is still deciding, and a preview that consumed the entry would strand
///   the directory forever when they pressed Cancel.
/// * **Only [`RemovalPolicy::KeepIfDirty`] is reported.** A `Force` worktree is
///   removed regardless, so there is nothing kept to tell anyone about.
/// * **The probe is time-boxed** by `probe_timeout`, because this sits on an
///   interactive key path while `remove_worktree`'s copy does not. A probe that
///   does not finish downgrades to `confirmed_dirty: false` rather than
///   dropping the warning — the tree is kept either way, so the useful half of
///   the report (the path) survives a slow checkout. The timeout abandons the
///   `git` child rather than killing it, and deliberately: `git status`
///   refreshes the index as it walks, so a SIGKILL mid-write is a worse
///   outcome than letting a short read-only process finish and be reaped by
///   tokio's orphan queue.
pub async fn kept_worktree_preview(
    records: &[AgentRecord],
    worktrees: &WorktreeRegistry,
    pane_ids: &[String],
    probe_timeout: Duration,
) -> Option<KeptWorktree> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for pane_id in pane_ids {
        let Some(record) = records
            .iter()
            .find(|r| r.pane_id_env.as_deref() == Some(pane_id.as_str()))
        else {
            continue;
        };
        let Some(worktree) = worktree_of_record(record) else {
            continue;
        };
        if seen.contains(&worktree) {
            continue;
        }
        seen.push(worktree.clone());
        let policy = worktrees
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&worktree)
            .map(|entry| entry.policy);
        if policy != Some(RemovalPolicy::KeepIfDirty) {
            continue;
        }
        let confirmed_dirty =
            match tokio::time::timeout(probe_timeout, worktree_is_dirty(&worktree)).await {
                Ok(Ok(true)) => true,
                // Clean: this tree is about to be REMOVED, and saying so would be
                // the noise that makes the warning worth ignoring. Say nothing.
                Ok(Ok(false)) => continue,
                // A failed probe is what `remove_worktree` itself treats as a
                // reason to keep, so report the path — under wording that does not
                // claim more than was measured.
                Ok(Err(e)) => {
                    tracing::debug!(
                        worktree = %worktree.display(),
                        error = %e,
                        "close preview: could not check worktree status"
                    );
                    false
                }
                // A blown deadline is not an answer either way: the removal path
                // runs the same probe with no deadline and may still find it clean.
                // Report conditionally rather than dropping the path.
                Err(_) => {
                    tracing::debug!(
                        worktree = %worktree.display(),
                        "close preview: worktree status probe timed out"
                    );
                    false
                }
            };
        return Some(KeptWorktree {
            path: worktree.to_string_lossy().into_owned(),
            confirmed_dirty,
        });
    }
    None
}

/// Remove a dispatched worktree from its clone (`git -C <clone> worktree remove
/// <worktree>`), PRESERVING the clone. Best-effort: a non-zero exit (already
/// removed, locked) or a spawn error is logged, not fatal — the tab is already
/// gone.
///
/// `policy` decides what happens when the worktree still holds uncommitted work
/// — see [`RemovalPolicy`] for why the two producers need opposite answers.
/// Under [`RemovalPolicy::KeepIfDirty`] a dirty tree (or a status probe that
/// fails, so dirtiness is unknown) is left in place and logged; under
/// [`RemovalPolicy::Force`] the tree is removed regardless, which is what keeps
/// PRD #120's vacated slot reclaimable.
///
/// Issue #717: returns `Some` exactly when a tree was KEPT rather than removed,
/// so the caller can tell the user. This is the AUTHORITATIVE answer, and the
/// only one that is: it is measured after `close_agent` has reaped the agent, so
/// nothing is writing to the tree any more and the verdict cannot go stale. The
/// close dialog's [`kept_worktree_preview`] runs while the agent is still alive
/// and is therefore a prediction — a useful one, because it arrives while the
/// user can still cancel, but not a fact.
pub async fn remove_worktree(
    worktree_dir: &Path,
    clone_dir: &Path,
    policy: RemovalPolicy,
) -> Option<KeptWorktree> {
    let worktree = worktree_dir.to_string_lossy();
    if policy == RemovalPolicy::KeepIfDirty {
        match worktree_is_dirty(worktree_dir).await {
            Ok(true) => {
                tracing::warn!(
                    worktree = %worktree_dir.display(),
                    "dispatch: worktree has uncommitted changes; leaving in place"
                );
                return Some(KeptWorktree {
                    path: worktree_dir.to_string_lossy().into_owned(),
                    confirmed_dirty: true,
                });
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    worktree = %worktree_dir.display(),
                    error = %e,
                    "dispatch: could not check worktree status; leaving in place"
                );
                return Some(KeptWorktree {
                    path: worktree_dir.to_string_lossy().into_owned(),
                    confirmed_dirty: false,
                });
            }
        }
    }

    let clone = clone_dir.to_string_lossy();
    let mut args = vec!["-C", &clone, "worktree", "remove", &worktree];
    if policy == RemovalPolicy::Force {
        args.push("--force");
    }
    let res = run_status("git", &args).await;
    match res {
        Ok(()) => {
            tracing::info!(
                worktree = %worktree_dir.display(),
                "issue-dispatch: removed worktree on tab close (clone preserved)"
            );
            None
        }
        // A FAILED removal leaves the directory on disk too, so the user is
        // told about it for the same reason a deliberate keep is: something
        // they may care about is still there. `confirmed_dirty` is false —
        // nothing measured it — which is also the wording that fits, since the
        // reason is a stuck `git worktree remove` rather than their edits.
        Err(e) => {
            tracing::warn!(
                worktree = %worktree_dir.display(),
                error = %e,
                "issue-dispatch: worktree cleanup on close failed"
            );
            Some(KeptWorktree {
                path: worktree_dir.to_string_lossy().into_owned(),
                confirmed_dirty: false,
            })
        }
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
    registry: &Arc<AgentPtyRegistry>,
    worktrees: &WorktreeRegistry,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    // The daemon's `AppState`, so an issue-dispatched ORCHESTRATION's roles are
    // registered for delegate routing — see
    // `crate::state::AppState::register_orchestration_role`.
    state: Option<&crate::state::SharedState>,
) {
    // S5 — every derived path (clone, worktree, the spawn's orchestration_cwd)
    // must be absolute: a relative workspace root would double-nest the worktree
    // under `git -C <clone> worktree add <relative>` and drop orchestration_cwd
    // (`is_valid_orchestration_cwd` requires an absolute path) → no tab-close
    // cleanup. The schedules loader already resolves relatives against $HOME, so a
    // non-absolute value here is a misconfiguration: reject this run.
    let workspace = match canonical_workspace(working_dir) {
        Ok(p) => p,
        Err(message) => {
            notifier.notify(NotifyEvent::IssueDispatchRepoError {
                task: task_name.to_string(),
                repo: cfg.repo.clone(),
                message,
            });
            return;
        }
    };
    // L2 + S4 — the clone-dir path component is a SANITIZED single segment of the
    // task name (never `/`, `..`, or absolute), so it can't nest or escape the
    // workspace. Identical to `derive_issue_paths(..).clone_dir`.
    let clone_dir = workspace.join(crate::issue_dispatch::sanitize_clone_segment(task_name));

    // M2.1 — provision the repo clone (clone-if-missing / fetch+ff-pull-if-present).
    if let Err(message) = provision_repo(&workspace, &clone_dir, &cfg.repo).await {
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

    // S2 — `max_per_run` caps the issues CONSIDERED per run (not the number newly
    // dispatched): already-claimed issues inside the cap are skipped, yielding a
    // clean "≤ max_per_run concurrent in-flight" ceiling (PRD concurrency model —
    // today's run only picks up slots yesterday's run vacated).
    for issue in issues.into_iter().take(cfg.max_per_run) {
        // M3.2 — per-issue error boundary: one failure never aborts the rest.
        if let Err(message) = dispatch_one_issue(
            task_name,
            &workspace,
            prompt_template,
            cfg,
            default_command.as_deref(),
            issue,
            &clone_dir,
            registry,
            worktrees,
            notifier,
            event_tx,
            state,
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
    workspace: &Path,
    prompt_template: &str,
    cfg: &IssueDispatchConfig,
    default_command: Option<&str>,
    issue: u64,
    clone_dir: &Path,
    registry: &Arc<AgentPtyRegistry>,
    worktrees: &WorktreeRegistry,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    // Threaded to `spawn` so an issue-dispatched ORCHESTRATION's roles land in
    // the daemon's delegate-routing maps — see
    // `crate::state::AppState::register_orchestration_role`.
    state: Option<&crate::state::SharedState>,
) -> Result<(), String> {
    let paths = derive_issue_paths(workspace, task_name, issue);

    let notify_skip = || {
        notifier.notify(NotifyEvent::IssueDispatchSkipped {
            task: task_name.to_string(),
            repo: cfg.repo.clone(),
            issue,
            branch: paths.branch.clone(),
        });
    };

    // M2.2 — idempotency BEFORE any work, evaluated as a SHORT-CIRCUIT on the
    // two signals so the secondary check only runs when the primary leaves the
    // verdict open.
    //
    // PRIMARY (the worktree is the ledger): if the per-issue worktree already
    // exists the issue is already claimed — emit a SKIP and return IMMEDIATELY,
    // WITHOUT consulting the open-PR signal. Probing `issue_has_open_pr` here
    // would be both redundant (a present worktree skips regardless of the PR
    // check) and a correctness hazard: a transient `gh pr list` failure would,
    // via the per-issue error boundary, turn this clean SKIP into a spurious
    // IssueDispatchFailed notification.
    let worktree_exists = paths.worktree_dir.exists();
    if worktree_exists {
        notify_skip();
        return Ok(());
    }

    // SECONDARY — reached ONLY when the worktree is absent: an open PR whose
    // head is `agent/issue-<n>`. A `gh` failure here is a genuine per-issue
    // error (e.g. the stub's simulated API error) and propagates via `?`.
    let open_pr = issue_has_open_pr(&cfg.repo, issue).await?;
    if dispatch_decision(worktree_exists, open_pr) == DispatchDecision::Skip {
        notify_skip();
        return Ok(());
    }

    // M2.2 — create the per-issue worktree on `agent/issue-<n>`. A concurrent
    // fire can claim it in the TOCTOU window after the idempotency check above
    // (see `create_worktree`); that benign race is a skip, not a failure —
    // mirroring the `dispatch_decision` worktree-presence skip.
    match create_worktree(
        clone_dir,
        &paths.worktree_dir,
        &paths.branch,
        true,
        Creator::issue_dispatch(task_name, issue),
    )
    .await?
    {
        WorktreeCreation::Created => {}
        // `reuse_existing_branch: true` above means `BranchExists` is never
        // returned to this caller — an existing `agent/issue-<n>` is ATTACHED,
        // which is exactly what keeps the vacated slot reclaimable. Treated as a
        // skip alongside `AlreadyClaimed` so the match stays exhaustive if that
        // ever changes.
        WorktreeCreation::AlreadyClaimed | WorktreeCreation::BranchExists => {
            notifier.notify(NotifyEvent::IssueDispatchSkipped {
                task: task_name.to_string(),
                repo: cfg.repo.clone(),
                issue,
                branch: paths.branch.clone(),
            });
            return Ok(());
        }
    }

    // M2.4 — record the worktree for tab-close cleanup NOW, before the spawn's
    // prompt-delivery wait. `spawn` registers the agent (visible to a `StopAgent`
    // from a fast client) well before it returns, so recording after the spawn
    // would race a prompt close. The close watcher matches the agent to this
    // worktree by its record's cwd, not by an agent id we don't have yet.
    // `RemovalPolicy::Force`: this worktree lives inside a daemon-owned clone,
    // and the reuse-the-vacated-slot model depends on the directory actually
    // going away on tab close — a tree left behind makes `dispatch_decision`
    // skip the issue on every later fire. See [`RemovalPolicy`].
    record_worktree(
        worktrees,
        &paths.worktree_dir,
        clone_dir,
        RemovalPolicy::Force,
    );

    // M2.3 — spawn one agent into the worktree, delivering the substituted
    // prompt. `spawn` branches on the worktree's `.dot-agent-deck.toml`.
    //
    // `detach_delivery = true`: the agent is still registered synchronously (so
    // the idempotency/worktree state is consistent the moment this returns), but
    // the prompt-delivery wait — which can sit out the multi-second `SessionStart`
    // fallback for a hook-less command — runs in the background. This frees the
    // scheduler's run-active window as soon as the dispatch WORK is done, so a
    // re-fire right after a tab close (PRD #120 B1 / dispatch/008) isn't skipped
    // behind the prior run's lingering delivery wait. The worktree-on-disk
    // idempotency signal still serializes overlapping fires safely.
    let req = SpawnRequest {
        task_name: task_name.to_string(),
        working_dir: paths.worktree_dir.to_string_lossy().into_owned(),
        command: default_command.map(str::to_string),
        prompt: substitute_issue_number(prompt_template, issue),
        // `None`: issue-dispatch keeps deriving the shape from the cloned repo's
        // own config, exactly as before the PRD #220 selector existed.
        resolved_target: None,
        // Unchanged behaviour: the prompt is delivered verbatim. Giving this path
        // the orchestrator context is #222's work, not this PR's.
        compose_orchestrator_context: false,
    };
    if let Err(e) = spawn(req, registry, notifier, event_tx, true, state).await {
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

/// S5: resolve the task's `working_dir` to an ABSOLUTE workspace root. The
/// schedules loader already expands `~`/`$VAR` and resolves relatives against
/// `$HOME`, so a non-absolute value reaching the dispatch flow is a
/// misconfiguration — reject it rather than silently resolving against the
/// daemon's cwd (which would derive the wrong clone/worktree paths and drop
/// orchestration cleanup). An absolute input is normalized via
/// [`std::path::absolute`].
fn canonical_workspace(working_dir: &str) -> Result<PathBuf, String> {
    let p = Path::new(working_dir);
    if !p.is_absolute() {
        return Err(format!(
            "working_dir {working_dir:?} is not absolute; issue-dispatch requires an absolute \
             workspace root"
        ));
    }
    std::path::absolute(p)
        .map_err(|e| format!("failed to absolutize working_dir {working_dir:?}: {e}"))
}

/// M2.1: clone the repo if its dir is missing, else refresh the existing clone
/// (fetch + fast-forward pull). `gh` / `git` are resolved from `PATH` and inherit
/// the daemon's environment.
///
/// L3 (fail-closed): before touching a pre-existing clone dir, verify it is OUR
/// clone of `repo` by reading its `origin` — a missing origin (not a clone) or a
/// github.com origin for a DIFFERENT repo aborts the run without fetching,
/// pulling, writing, or deleting the dir.
///
/// S3: a refresh failure on an EXISTING clone is non-fatal — worktrees branch off
/// whatever refs are already on disk, so a transient `fetch`/`pull` error is
/// logged and the run continues. A MISSING clone that fails to clone stays fatal
/// (the run can't proceed without the repo).
async fn provision_repo(workspace: &Path, clone_dir: &Path, repo: &str) -> Result<(), String> {
    if clone_dir.is_dir() {
        let clone = clone_dir.to_string_lossy();
        let origin = run_capture_args("git", &["-C", &clone, "remote", "get-url", "origin"])
            .await
            .map_err(|e| {
                format!(
                    "clone dir {} has no usable git origin; refusing to refresh a foreign dir: {e}",
                    clone_dir.display()
                )
            })?;
        let origin = origin.trim();
        if !origin_matches_repo(origin, repo) {
            return Err(format!(
                "clone dir {} has origin {origin:?}, which does not match configured repo \
                 {repo:?}; refusing to fetch/pull (fail-closed)",
                clone_dir.display()
            ));
        }
        if let Err(e) = refresh_clone(&clone).await {
            tracing::warn!(
                clone = %clone_dir.display(),
                error = %e,
                "issue-dispatch: clone refresh failed; continuing with current refs"
            );
        }
        // Keep the per-issue `.worktrees/` dir out of the clone's `git status`
        // (idempotent, best-effort — never fails the run).
        ensure_worktrees_excluded(clone_dir);
        return Ok(());
    }
    std::fs::create_dir_all(workspace)
        .map_err(|e| format!("failed to create workspace {}: {e}", workspace.display()))?;
    run_status("gh", &["repo", "clone", repo, &clone_dir.to_string_lossy()]).await?;
    // Same hygiene on the fresh clone, so it holds across the first AND every
    // later fire.
    ensure_worktrees_excluded(clone_dir);
    Ok(())
}

/// Keep the per-issue worktrees dir (`<clone>/.worktrees/`) out of the clone's
/// `git status` WITHOUT touching the user's tracked files: append `.worktrees/`
/// to the clone's LOCAL exclude file (`<clone>/.git/info/exclude`) — never a
/// committed `.gitignore`, because the cloned repo belongs to the user and we
/// must not modify their tracked/committed files. `.worktrees/` sits in the main
/// clone's working tree and would otherwise show as untracked to anyone running
/// `git status` in the clone (agents run INSIDE a worktree, above which it isn't
/// visible — so this is hygiene for the main clone).
///
/// Idempotent: the line is appended only if not already present, so repeated
/// fires never duplicate it; `.git/info/` is created if missing. Best-effort: any
/// I/O failure is logged at WARN and swallowed — it must NEVER fail the dispatch
/// run.
fn ensure_worktrees_excluded(clone_dir: &Path) {
    const WORKTREES_EXCLUDE_LINE: &str = ".worktrees/";
    let info_dir = clone_dir.join(".git").join("info");
    let exclude_path = info_dir.join("exclude");

    // A missing exclude reads as empty — treat that as "line absent".
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim() == WORKTREES_EXCLUDE_LINE)
    {
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&info_dir) {
        tracing::warn!(
            clone = %clone_dir.display(),
            error = %e,
            "issue-dispatch: could not create .git/info to exclude .worktrees/"
        );
        return;
    }

    // Append on its own line, inserting a separating newline only when the
    // existing content lacks a trailing one.
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(WORKTREES_EXCLUDE_LINE);
    content.push('\n');
    if let Err(e) = std::fs::write(&exclude_path, content) {
        tracing::warn!(
            clone = %clone_dir.display(),
            error = %e,
            "issue-dispatch: could not write .git/info/exclude to exclude .worktrees/"
        );
    }
}

/// S3: refresh an existing clone in place — `git fetch` then `git pull --ff-only`.
/// The caller treats any failure here as non-fatal (warn + continue).
async fn refresh_clone(clone: &str) -> Result<(), String> {
    run_status("git", &["-C", clone, "fetch"]).await?;
    run_status("git", &["-C", clone, "pull", "--ff-only"]).await
}

/// L3: whether an existing clone's `origin` is consistent with the configured
/// `repo`. A recognizable github.com origin must resolve to the same
/// `owner/name` (case-insensitive); a non-github origin — a self-hosted host or
/// the local fixture remote used in tests — cannot be attributed to an
/// `owner/name`, so it is accepted (we provisioned it). The strict case this
/// guards is a clone-dir collision where `origin` points at a DIFFERENT GitHub
/// repo than configured.
fn origin_matches_repo(origin: &str, repo: &str) -> bool {
    match github_owner_name(origin) {
        Some(found) => found == repo.to_ascii_lowercase(),
        None => true,
    }
}

/// Normalize a github.com remote URL to lowercase `owner/name`, or `None` if it
/// is not a recognizable github.com remote (other hosts, local paths, …).
/// Handles the `https://`, `http://`, `ssh://git@`, `git://`, and `git@…:` forms,
/// with or without a trailing `.git`.
fn github_owner_name(origin: &str) -> Option<String> {
    let s = origin.trim();
    let rest = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("ssh://git@github.com/"))
        .or_else(|| s.strip_prefix("git://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
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
    parse_open_pr_present(&stdout)
}

/// N1: parse `gh pr list --json number` into "is there an open PR?". Malformed
/// output (invalid JSON, or valid JSON that is NOT an array) PROPAGATES as an
/// error — symmetric with [`parse_issue_numbers`] — so the per-issue boundary
/// skips + logs the issue (fail-safe) rather than silently reading it as "no PR
/// → dispatch", which would risk a duplicate dispatch.
fn parse_open_pr_present(json: &str) -> Result<bool, String> {
    let value: serde_json::Value = serde_json::from_str(json.trim())
        .map_err(|e| format!("failed to parse `gh pr list` JSON: {e}"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| "`gh pr list` did not return a JSON array".to_string())?;
    Ok(!arr.is_empty())
}

/// Outcome of [`create_worktree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCreation {
    /// The worktree was created.
    Created,
    /// The worktree DIRECTORY is already there — a concurrent fire claimed it in
    /// the benign TOCTOU window described below. Callers surface this as a skip
    /// rather than a failure.
    AlreadyClaimed,
    /// The worktree directory is absent but the head BRANCH already exists, and
    /// the caller asked not to reuse it (`reuse_existing_branch: false`).
    ///
    /// Distinct from [`Self::AlreadyClaimed`] because the two need different
    /// messages and have different fixes: "another dispatch is using this" (wait
    /// or pick another name) versus "a previous dispatch left this branch
    /// behind" (delete the branch, or pick another name). Collapsing them made
    /// a reused name report a worktree conflict that the user could see was not
    /// true — the directory is plainly gone — with no hint of the real cause.
    BranchExists,
}

/// Attempts (the first included) at `git worktree add` when it fails because a
/// concurrent add's administrative directory was only half written — issue #541.
/// Bounded on purpose: the window is microseconds wide in the wild, so a handful
/// of tries covers a genuine race by orders of magnitude, while a `commondir`
/// that is permanently unreadable still surfaces as an error instead of being
/// retried forever.
const WORKTREE_ADD_ATTEMPTS: u32 = 5;

/// Backoff before retry `attempt` (1-based): 100ms, 200ms, 400ms, 800ms — 1.5s
/// of cover in total, which is ~six orders of magnitude more than the two-syscall
/// window it exists for, and is also the entire latency a genuinely broken repo
/// pays before its error is reported.
fn worktree_add_backoff(attempt: u32) -> Duration {
    // Saturating and capped so the arithmetic stays total: at five attempts the
    // shift never exceeds 3, but raising [`WORKTREE_ADD_ATTEMPTS`] must not be
    // able to turn a backoff into an overflow panic.
    Duration::from_millis(100u64 << attempt.saturating_sub(1).min(10))
}

/// How long to wait for the per-repository worktree lock before giving up on
/// serialization and creating the worktree unserialized. A stuck holder must
/// slow a dispatch down, never wedge it forever.
const WORKTREE_LOCK_WAIT: Duration = Duration::from_secs(60);

/// Issue #541: does this `git worktree add` failure look like the reader side of
/// a concurrent add rather than a real problem?
///
/// `git worktree add` scans the repo's worktree list before creating its own
/// entry, reading every entry's `commondir`. An add that has created its entry
/// but not yet written that file makes the read come back short, and git turns
/// that into `fatal: failed to read '<…>/worktrees/<name>/commondir': Success` —
/// `strerror(errno)` for an errno nothing ever set, which is the tell that the
/// read did not actually fail.
///
/// Keyed on the FILE NAME, not on git's sentence: both the `die_errno` format
/// string and `strerror` are localized, so "failed to read" and "Success"
/// disappear under a non-English locale while `commondir` — a path component —
/// does not. Deliberately narrow all the same: it matches only failures naming
/// that one administrative file, and even those are retried a bounded number of
/// times.
fn is_worktree_scan_short_read(err: &str) -> bool {
    err.contains("commondir")
}

/// Path of the lock that serializes worktree creation for one repository.
///
/// Derived from `git rev-parse --git-common-dir` and placed inside it, because
/// that directory is exactly the contended resource — `worktrees/<name>` lives
/// there — and because `clone_dir` may itself be a linked worktree, whose `.git`
/// is a FILE and whose per-worktree admin dir is private. Asking git means every
/// spelling of one repository (the main checkout, any of its worktrees, a
/// relative path) maps onto a single lock file, which is what makes the
/// exclusion hold between separate deck PROCESSES and not merely between tasks
/// in one.
///
/// Returns `None` when the directory cannot be resolved (not a git repo) — the
/// add itself then fails with git's own message, which is the better error.
async fn worktree_lock_path(clone_dir: &Path) -> Option<PathBuf> {
    let common = run_capture_args(
        "git",
        &[
            "-C",
            &clone_dir.to_string_lossy(),
            "rev-parse",
            "--git-common-dir",
        ],
    )
    .await
    .ok()?;
    let common = common.trim();
    if common.is_empty() {
        return None;
    }
    // `--git-common-dir` answers relative to the repository (plain `.git` for an
    // ordinary checkout), and our cwd is the daemon's, not `clone_dir`'s — so a
    // relative answer has to be joined here. Resolved this way rather than with
    // `--path-format=absolute` so the derivation does not depend on git 2.31+.
    let common = Path::new(common);
    let path = if common.is_absolute() {
        common.to_path_buf()
    } else {
        clone_dir.join(common)
    };
    Some(path.join("dot-agent-deck-worktree.lock"))
}

/// Serialize `git worktree add` for one repository across processes (issue
/// #541), so this deck's own concurrent dispatches cannot observe each other's
/// half-created administrative directories.
///
/// Every failure mode degrades to "create the worktree anyway": an unresolvable
/// repo, an unwritable `.git`, or a holder that never lets go all return `None`,
/// which is precisely the pre-#541 behaviour that the bounded retry in
/// [`create_worktree`] already covers. A lock is worth having only while it
/// cannot itself become the reason a dispatch fails or hangs.
///
/// Serialization also does not make the retry redundant, and vice versa: this
/// only binds processes that take the lock, so an add started by the user or by
/// another tool still races us, and only the retry survives that.
async fn acquire_worktree_lock(clone_dir: &Path) -> Option<crate::platform::lock::SpawnLock> {
    let path = worktree_lock_path(clone_dir).await?;
    match tokio::time::timeout(
        WORKTREE_LOCK_WAIT,
        crate::platform::lock::acquire_spawn_lock(&path),
    )
    .await
    {
        Ok(Ok(guard)) => Some(guard),
        Ok(Err(e)) => {
            tracing::warn!(
                lock = %path.display(),
                error = %e,
                "could not take the per-repository worktree lock; creating the worktree \
                 unserialized (the bounded retry still covers a concurrent add)"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                lock = %path.display(),
                waited_secs = WORKTREE_LOCK_WAIT.as_secs(),
                "timed out waiting for the per-repository worktree lock; creating the \
                 worktree unserialized rather than wedging the dispatch"
            );
            None
        }
    }
}

/// M2.2: create the per-issue worktree on `agent/issue-<n>`. The `.worktrees`
/// parent is created first so the add never trips on a missing dir.
///
/// B1: `git worktree remove` PRESERVES the branch, so an issue that was
/// dispatched, had its tab closed without a PR, and is still open leaves
/// `agent/issue-<n>` behind. A naive `worktree add -b <branch>` would then fail
/// ("a branch named … already exists") on EVERY later fire, permanently wedging
/// the reuse-the-vacated-slot model. So probe for the branch first: when
/// `reuse_existing_branch` is true, attach the existing branch (no `-b`) when it is
/// already there, and only create it (`-b`) when it is not. When
/// `reuse_existing_branch` is false, an existing branch is reported as
/// [`WorktreeCreation::BranchExists`] so the caller can refuse the dispatch and
/// say WHY — the branch may hold committed work from a previous dispatch of the
/// same name, so it is never deleted implicitly.
///
/// TOCTOU: the caller only reaches here after [`dispatch_decision`] saw the
/// worktree dir ABSENT, but a concurrent fire of the same task can create it in
/// the window before this `worktree add` runs — the add then fails on the now-
/// present path. Because we only arrive with the dir believed absent, its
/// presence after a failed add means a concurrent claim, not our error: report
/// [`WorktreeCreation::AlreadyClaimed`] (→ skip) instead of a hard failure. A
/// genuine add failure (bad ref, permissions, …) leaves the dir absent and
/// still propagates as `Err`.
///
/// Issue #541 — a SECOND, unrelated concurrency hazard, on a different name.
/// The one above is two dispatches racing for the same worktree; this one is any
/// two adds racing on the same *repository*, and it fails the loser even though
/// nothing about its dispatch is wrong. `git worktree add` scans the repo's
/// worktree list before creating its own entry and reads every entry's
/// `commondir`; a concurrent add that has created its entry but not yet written
/// that file makes the read come back short, and the loser dies with `fatal:
/// failed to read '…/worktrees/<name>/commondir': Success`. Two defences, and
/// each covers what the other cannot:
///
/// - [`acquire_worktree_lock`] serializes the adds this deck starts, per
///   repository and across processes, so the deck stops being its own worst
///   offender (three concurrent dispatches — `scheduler/dispatch/015` — is the
///   reported case).
/// - [`is_worktree_scan_short_read`] + [`WORKTREE_ADD_ATTEMPTS`] retry the one
///   transient signature a bounded number of times, which is the only thing that
///   can help against an add the deck did not start (the user's own, another
///   tool's) since those take no lock.
///
/// Neither defence swallows anything: a `commondir` that stays unreadable
/// exhausts the attempts and surfaces as `Err`.
///
/// Issue #425 — `creator`. This is the ONLY `git worktree add` in `src/`, so
/// it is also the only place that can claim a worktree as the deck's own at
/// the moment it comes into existence. On success it writes the ownership
/// marker `worktree_reclaim` later reads, recording `creator` so the claim
/// names the responsible dispatch rather than a bare "the deck". Written on
/// the `Created` arm only, and best-effort — see [`crate::worktree_owner`] for
/// why both of those are load-bearing.
pub async fn create_worktree(
    clone_dir: &Path,
    worktree_dir: &Path,
    branch: &str,
    reuse_existing_branch: bool,
    creator: Creator,
) -> Result<WorktreeCreation, String> {
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create worktree parent {}: {e}", parent.display()))?;
    }
    // Issue #541: keep this deck's own concurrent creations off each other's
    // half-created entries. Held across the probe AND the add so the two are
    // atomic with respect to another dispatch of the same name; released on drop
    // at the end of the function.
    let _repo_lock = acquire_worktree_lock(clone_dir).await;

    let clone = clone_dir.to_string_lossy();
    let wt = worktree_dir.to_string_lossy();
    let branch_ref = format!("refs/heads/{branch}");
    let mut attempt: u32 = 1;
    let add = loop {
        // Re-probed on every attempt, not hoisted out of the loop: a `git
        // worktree add` that dies on the scan has already CREATED its `-b`
        // branch (the branch survives the exit-128), so passing `-b` again would
        // fail with "a branch named … already exists" and turn a transient race
        // into a hard failure — and, with `reuse_existing_branch: false`, into a
        // dispatch name the user has to `git branch -D` by hand.
        let branch_exists = run_status(
            "git",
            &[
                "-C",
                &clone,
                "rev-parse",
                "--verify",
                "--quiet",
                &branch_ref,
            ],
        )
        .await
        .is_ok();
        // Only attempt 1 can report BranchExists. Reaching attempt 2 means the
        // branch was PROVEN absent moments ago, so anything there now was
        // created either by our own failed attempt or by a dispatch racing us —
        // never the "may hold committed work from an earlier dispatch" case this
        // guard exists for. (A branch a racing dispatch is really using fails the
        // attach with "already used by worktree at …", and its worktree dir is
        // then present, so the outcome is `AlreadyClaimed` below.)
        if branch_exists && !reuse_existing_branch && attempt == 1 {
            // …but only when the worktree really IS gone, which is precisely what
            // the BranchExists message asserts ("its worktree is already gone",
            // `dispatch.rs`). A directory that is present is a live claim, and
            // saying otherwise sends the user to `git branch -D` for a worktree
            // they can see. Serializing creation made this reachable by design
            // rather than by luck: the loser of a same-name race now always
            // probes AFTER the winner created the branch, where before it might
            // have probed first and been classified (correctly) as
            // `AlreadyClaimed` by the post-add check below.
            if worktree_dir.exists() {
                return Ok(WorktreeCreation::AlreadyClaimed);
            }
            return Ok(WorktreeCreation::BranchExists);
        }
        let result = if branch_exists {
            run_status("git", &["-C", &clone, "worktree", "add", &wt, branch]).await
        } else {
            run_status("git", &["-C", &clone, "worktree", "add", &wt, "-b", branch]).await
        };
        match result {
            Err(e) if attempt < WORKTREE_ADD_ATTEMPTS && is_worktree_scan_short_read(&e) => {
                let backoff = worktree_add_backoff(attempt);
                tracing::warn!(
                    clone = %clone_dir.display(),
                    worktree = %worktree_dir.display(),
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "git worktree add read another add's half-created administrative \
                     directory (issue #541); retrying after a backoff"
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            // Success, a non-transient failure, or the last attempt: the retry
            // is bounded, so a `commondir` that is genuinely unreadable (a
            // crashed add left an empty one behind) still surfaces as an error
            // rather than looping or being swallowed.
            other => break other,
        }
    };
    match add {
        Ok(()) => {
            // Issue #425: claim the worktree we just created, HERE, so no
            // window exists in which a deck-created worktree is
            // unrecognisable to `worktree list|reclaim`.
            //
            // Only on this arm. `AlreadyClaimed` below means the directory was
            // already on disk when our add ran, so somebody else created it —
            // marking it would be the deck asserting ownership of a directory
            // it did not create, which is the one failure the marker exists to
            // prevent on a path that deletes directories. (A concurrent
            // dispatch that really did create it writes its own marker from
            // its own `Created` arm, so nothing is lost.)
            //
            // Best-effort by construction: `write_marker_best_effort` warns
            // and returns rather than failing the creation. A missing marker
            // costs one `--yes` confirmation later, which is the fail-safe
            // direction; a failed dispatch is not.
            crate::worktree_owner::write_marker_best_effort(worktree_dir, branch, creator).await;
            Ok(WorktreeCreation::Created)
        }
        // Concurrent claim (TOCTOU): the dir is present now though we arrived
        // believing it absent — treat as already-claimed. A real failure leaves
        // the dir absent and surfaces as the original error.
        Err(e) => {
            if worktree_dir.exists() {
                Ok(WorktreeCreation::AlreadyClaimed)
            } else {
                Err(e)
            }
        }
    }
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
pub async fn run_status(program: &str, args: &[&str]) -> Result<(), String> {
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
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_capture_args(program, &refs).await
}

/// Like [`run_capture`] but for `&str` args — the fixed-shape `git` probes
/// (e.g. `remote get-url origin`) build their argv inline.
async fn run_capture_args(program: &str, args: &[&str]) -> Result<String, String> {
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
        record_worktree(&reg, &wt7, &clone, RemovalPolicy::Force);
        record_worktree(&reg, &wt8, &clone, RemovalPolicy::Force);

        // The registry primitive returns a recorded worktree's entry exactly
        // once, then drops it (a re-take finds nothing). The close watcher
        // only calls `take_worktree` after `worktree_still_in_use` confirms the
        // last rooted agent has closed, so this once-only take is correct even
        // for a multi-role tab. issue-8 is untouched.
        let taken = take_worktree(&reg, &wt7).expect("issue-7 was recorded");
        assert_eq!(taken.clone_dir, clone);
        assert_eq!(taken.policy, RemovalPolicy::Force);
        assert_eq!(take_worktree(&reg, &wt7), None);
        assert_eq!(take_worktree(&reg, &wt8).map(|e| e.clone_dir), Some(clone));
    }

    // --- issue #717: the kept-worktree close preview ---

    /// A real repo with one commit plus a linked worktree at `worktree`, so the
    /// preview's `git status --porcelain` runs against a genuine tree rather
    /// than a stub. Mirrors `dispatch::tests::init_repo`.
    fn init_repo_with_worktree(repo: &Path, worktree: &Path) {
        let run = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git available");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        std::fs::create_dir_all(repo).unwrap();
        run(repo, &["init", "-q", "."]);
        run(repo, &["config", "user.email", "t@t.t"]);
        run(repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-qm", "init"]);
        run(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt",
                &worktree.to_string_lossy(),
            ],
        );
    }

    /// One registered pane whose cwd is `worktree`.
    fn pane_in(pane_id: &str, worktree: &Path) -> AgentRecord {
        let mut r = record(Some(&worktree.to_string_lossy()), None);
        r.pane_id_env = Some(pane_id.to_string());
        r
    }

    const PROBE: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn kept_worktree_preview_reports_a_dirty_keep_if_dirty_tree_with_its_path() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-x");
        init_repo_with_worktree(&repo, &wt);
        std::fs::write(wt.join("scratch.txt"), "the user's uncommitted work").unwrap();

        let reg = new_worktree_registry();
        record_worktree(&reg, &wt, &repo, RemovalPolicy::KeepIfDirty);
        let records = vec![pane_in("pane-1", &wt)];

        let kept = kept_worktree_preview(&records, &reg, &["pane-1".to_string()], PROBE)
            .await
            .expect("a dirty KeepIfDirty tree must be previewed");
        assert_eq!(kept.path, wt.to_string_lossy());
        assert!(
            kept.confirmed_dirty,
            "the probe answered, so the report must say so flatly"
        );
        // The preview must PEEK, never consume: the user has not answered the
        // dialog yet, and a taken entry would strand the directory on Cancel.
        assert!(
            take_worktree(&reg, &wt).is_some(),
            "the preview must leave the registry entry for the close to act on"
        );
    }

    #[tokio::test]
    async fn kept_worktree_preview_is_silent_for_a_clean_tree() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-x");
        init_repo_with_worktree(&repo, &wt);

        let reg = new_worktree_registry();
        record_worktree(&reg, &wt, &repo, RemovalPolicy::KeepIfDirty);
        let records = vec![pane_in("pane-1", &wt)];

        assert_eq!(
            kept_worktree_preview(&records, &reg, &["pane-1".to_string()], PROBE).await,
            None,
            "a clean tree is REMOVED, so warning that it is kept would be false"
        );
    }

    #[tokio::test]
    async fn kept_worktree_preview_is_silent_for_a_force_removed_tree() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-x");
        init_repo_with_worktree(&repo, &wt);
        std::fs::write(wt.join("scratch.txt"), "discarded by design").unwrap();

        let reg = new_worktree_registry();
        // PRD #120 issue-dispatch: dirty or not, this tree goes.
        record_worktree(&reg, &wt, &repo, RemovalPolicy::Force);
        let records = vec![pane_in("pane-1", &wt)];

        assert_eq!(
            kept_worktree_preview(&records, &reg, &["pane-1".to_string()], PROBE).await,
            None,
            "a Force worktree is not kept, so there is nothing to report"
        );
    }

    #[tokio::test]
    async fn kept_worktree_preview_is_silent_for_an_ordinary_pane() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-x");
        init_repo_with_worktree(&repo, &wt);
        std::fs::write(wt.join("scratch.txt"), "dirty, but nobody dispatched here").unwrap();

        // Nothing recorded: this is a pane the user opened themselves, and the
        // deck removes no directory when it closes.
        let reg = new_worktree_registry();
        let records = vec![pane_in("pane-1", &wt)];

        assert_eq!(
            kept_worktree_preview(&records, &reg, &["pane-1".to_string()], PROBE).await,
            None,
            "an unregistered cwd is not a dispatched worktree"
        );
    }

    /// A multi-role orchestration shares ONE worktree across its role panes, so
    /// the preview must resolve it from whichever pane ids it is handed —
    /// including ones it does not recognise, which a tab close mixes in.
    #[tokio::test]
    async fn kept_worktree_preview_resolves_one_tree_from_any_of_a_tabs_panes() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-team");
        init_repo_with_worktree(&repo, &wt);
        std::fs::write(wt.join("scratch.txt"), "work from role 2").unwrap();

        let reg = new_worktree_registry();
        record_worktree(&reg, &wt, &repo, RemovalPolicy::KeepIfDirty);
        let orchestration = |pane_id: &str| {
            let mut r = record(
                Some("/ignored"),
                Some(TabMembership::Orchestration {
                    name: "team".into(),
                    role_index: 0,
                    role_name: "coder".into(),
                    is_start_role: false,
                    orchestration_cwd: Some(wt.to_string_lossy().into_owned()),
                    display_title: None,
                    orchestration_id: None,
                }),
            );
            r.pane_id_env = Some(pane_id.to_string());
            r
        };
        let records = vec![orchestration("role-0"), orchestration("role-1")];

        let panes = vec![
            "not-a-pane-the-daemon-knows".to_string(),
            "role-1".to_string(),
        ];
        let kept = kept_worktree_preview(&records, &reg, &panes, PROBE)
            .await
            .expect("an unknown pane id must not stop the known one from resolving");
        assert_eq!(kept.path, wt.to_string_lossy());
    }

    /// The probe's deadline degrades the WORDING, never the report: the tree is
    /// kept whether or not the status walk finished, and the path is the half
    /// the user actually needs.
    #[tokio::test]
    async fn kept_worktree_preview_still_reports_the_path_when_the_probe_times_out() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-x");
        init_repo_with_worktree(&repo, &wt);
        std::fs::write(wt.join("scratch.txt"), "work").unwrap();

        let reg = new_worktree_registry();
        record_worktree(&reg, &wt, &repo, RemovalPolicy::KeepIfDirty);
        let records = vec![pane_in("pane-1", &wt)];

        let kept = kept_worktree_preview(
            &records,
            &reg,
            &["pane-1".to_string()],
            Duration::from_nanos(1),
        )
        .await
        .expect("an unanswered probe must still report the path");
        assert_eq!(kept.path, wt.to_string_lossy());
        assert!(
            !kept.confirmed_dirty,
            "nothing was measured, so nothing may be claimed"
        );
    }

    /// Issue #717 (Greptile P1): the AUTHORITATIVE half of the report. The
    /// close dialog's preview is measured while the agent is still alive, so it
    /// can be overtaken — an agent that commits between the dialog and the close
    /// would make a reused "kept at <path>" claim false about a directory that
    /// was in fact removed. `remove_worktree` returns what it actually did,
    /// measured after the agent is reaped, and that is what reaches the user.
    #[tokio::test]
    async fn remove_worktree_reports_whether_it_kept_the_tree() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");

        // Dirty + KeepIfDirty: kept, and it says so with the path.
        let dirty = tmp.path().join("repo-dispatch-dirty");
        init_repo_with_worktree(&repo, &dirty);
        std::fs::write(dirty.join("scratch.txt"), "work").unwrap();
        let kept = remove_worktree(&dirty, &repo, RemovalPolicy::KeepIfDirty)
            .await
            .expect("a kept tree must be reported");
        assert_eq!(kept.path, dirty.to_string_lossy());
        assert!(kept.confirmed_dirty);
        assert!(dirty.is_dir(), "the tree must actually still be there");

        // Clean + KeepIfDirty: removed, and nothing is reported.
        let clean = tmp.path().join("repo-dispatch-clean");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git available");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        run(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "clean",
            &clean.to_string_lossy(),
        ]);
        assert_eq!(
            remove_worktree(&clean, &repo, RemovalPolicy::KeepIfDirty).await,
            None,
            "a removed tree must report nothing — there is nothing left to find"
        );
        assert!(!clean.exists());

        // Dirty + Force: removed regardless, and still nothing reported. The
        // report is about what was KEPT, and PRD #120's slot-reclaim model
        // depends on this one going away.
        let forced = tmp.path().join("repo-dispatch-forced");
        run(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "forced",
            &forced.to_string_lossy(),
        ]);
        std::fs::write(forced.join("scratch.txt"), "discarded by design").unwrap();
        assert_eq!(
            remove_worktree(&forced, &repo, RemovalPolicy::Force).await,
            None
        );
        assert!(!forced.exists());
    }

    /// A removal that FAILS also leaves the directory on disk, so the user is
    /// told for the same reason a deliberate keep is — under the conditional
    /// wording, because nothing measured their edits.
    #[tokio::test]
    async fn remove_worktree_reports_a_tree_left_behind_by_a_failed_removal() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-x");
        init_repo_with_worktree(&repo, &wt);

        // `git -C <clone> worktree remove` against a clone that is not a repo
        // fails, which is the shape of every real failure here (locked, busy,
        // already gone).
        let not_a_repo = tmp.path().join("not-a-repo");
        std::fs::create_dir_all(&not_a_repo).unwrap();
        let kept = remove_worktree(&wt, &not_a_repo, RemovalPolicy::Force)
            .await
            .expect("a failed removal leaves the tree, so it must be reported");
        assert_eq!(kept.path, wt.to_string_lossy());
        assert!(
            !kept.confirmed_dirty,
            "nothing measured the user's edits, so nothing may claim them"
        );
        assert!(wt.is_dir());
    }

    #[tokio::test]
    async fn worktree_is_dirty_matches_git_status_and_errs_when_it_cannot_ask() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("repo-dispatch-x");
        init_repo_with_worktree(&repo, &wt);

        assert_eq!(worktree_is_dirty(&wt).await, Ok(false));
        std::fs::write(wt.join("scratch.txt"), "work").unwrap();
        assert_eq!(worktree_is_dirty(&wt).await, Ok(true));

        // Unanswerable is an Err, never a `false` — both callers must be able to
        // tell "clean" from "could not ask", because only one of them is safe
        // to delete on.
        assert!(worktree_is_dirty(&tmp.path().join("gone")).await.is_err());
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
            // PRD #162: no live session state in this cwd-derivation fixture;
            // matches the registry's own `agent_records()` default (`None`).
            live: None,
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
                orchestration_id: None,
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

    // --- N1: pr-list parsing is symmetric with issue enumeration ---

    #[test]
    fn parse_open_pr_present_array_handling() {
        assert!(parse_open_pr_present(r#"[{"number":4242}]"#).unwrap());
        assert!(!parse_open_pr_present("[]\n").unwrap());
    }

    #[test]
    fn parse_open_pr_present_rejects_malformed_output() {
        // A non-array (valid JSON) and invalid JSON both PROPAGATE — not a silent
        // "no PR → dispatch".
        assert!(parse_open_pr_present("{}").is_err());
        assert!(parse_open_pr_present("not json").is_err());
    }

    // --- L3: origin attribution ---

    #[test]
    fn github_owner_name_normalizes_known_forms() {
        for url in [
            "https://github.com/Acme/Widgets.git",
            "https://github.com/Acme/Widgets",
            "http://github.com/acme/widgets",
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
            "git://github.com/acme/widgets",
        ] {
            assert_eq!(
                github_owner_name(url).as_deref(),
                Some("acme/widgets"),
                "failed to normalize {url:?}"
            );
        }
        // Non-github origins are not attributable.
        assert_eq!(github_owner_name("/tmp/ghstub/acme_widgets/remote"), None);
        assert_eq!(github_owner_name("https://gitlab.com/acme/widgets"), None);
        assert_eq!(github_owner_name("https://github.com/onlyowner"), None);
    }

    #[test]
    fn origin_matches_repo_fail_closed_on_github_mismatch_lenient_otherwise() {
        // Same GitHub repo (case-insensitive) → consistent.
        assert!(origin_matches_repo(
            "git@github.com:Acme/Widgets.git",
            "acme/widgets"
        ));
        // A DIFFERENT GitHub repo → rejected (fail-closed).
        assert!(!origin_matches_repo(
            "https://github.com/other/repo.git",
            "acme/widgets"
        ));
        // A non-github origin (the local fixture remote in tests) can't be
        // attributed → accepted.
        assert!(origin_matches_repo(
            "/tmp/ghstub/acme_widgets/remote",
            "acme/widgets"
        ));
    }

    // --- S1: shared-worktree last-close detection ---

    #[test]
    fn worktree_still_in_use_tracks_live_siblings() {
        let wt = Path::new("/ws/task/.worktrees/issue-7");
        let orch_in = |role: &str| {
            record(
                None,
                Some(TabMembership::Orchestration {
                    name: "o".into(),
                    role_index: 0,
                    role_name: role.into(),
                    is_start_role: role == "orchestrator",
                    orchestration_cwd: Some("/ws/task/.worktrees/issue-7".into()),
                    display_title: None,
                    orchestration_id: None,
                }),
            )
        };

        // Two role panes share the worktree → in use.
        let both = vec![orch_in("orchestrator"), orch_in("reviewer")];
        assert!(worktree_still_in_use(&both, wt));

        // After the reviewer closes, the orchestrator still roots it → in use.
        let one = vec![orch_in("orchestrator")];
        assert!(worktree_still_in_use(&one, wt));

        // After the last role closes → free. An unrelated agent doesn't count.
        let other = vec![record(Some("/somewhere/else"), None)];
        assert!(!worktree_still_in_use(&other, wt));
        assert!(!worktree_still_in_use(&[], wt));
    }

    // --- TOCTOU: concurrent-claim worktree race ---

    // PRD #120 — when the per-issue worktree dir is already present (a concurrent
    // fire claimed it in the window after the idempotency check), `create_worktree`
    // reports AlreadyClaimed so the caller skips the issue rather than failing it.
    // Deterministic: the production code keys solely on `worktree_dir.exists()`
    // after a failed `git worktree add`, so a non-git clone dir suffices to force
    // the add to fail; the pre-created worktree dir drives the already-claimed verdict.
    #[tokio::test]
    async fn create_worktree_already_claimed_when_dir_present() {
        let ws = tempfile::tempdir().unwrap();
        let clone_dir = ws.path().join("clone"); // not a git repo → add fails
        std::fs::create_dir_all(&clone_dir).unwrap();
        let worktree_dir = clone_dir.join(".worktrees").join("issue-7");
        // Simulate the concurrent fire having already created the worktree dir.
        std::fs::create_dir_all(&worktree_dir).unwrap();

        let outcome = create_worktree(
            &clone_dir,
            &worktree_dir,
            "agent/issue-7",
            false,
            Creator::issue_dispatch("unit", 7),
        )
        .await;
        assert_eq!(
            outcome,
            Ok(WorktreeCreation::AlreadyClaimed),
            "an already-present worktree dir is a concurrent claim → skip, not failure"
        );
    }

    // PRD #120 — a genuine `git worktree add` failure with NO worktree dir on disk
    // stays a hard failure (Err), so real problems (bad ref, permissions, …) are
    // still surfaced as IssueDispatchFailed rather than masked as a skip.
    #[tokio::test]
    async fn create_worktree_propagates_genuine_failure() {
        let ws = tempfile::tempdir().unwrap();
        let clone_dir = ws.path().join("clone"); // not a git repo → add fails
        std::fs::create_dir_all(&clone_dir).unwrap();
        let worktree_dir = clone_dir.join(".worktrees").join("issue-9"); // absent

        let outcome = create_worktree(
            &clone_dir,
            &worktree_dir,
            "agent/issue-9",
            false,
            Creator::issue_dispatch("unit", 9),
        )
        .await;
        assert!(
            outcome.is_err(),
            "a real add failure with no worktree on disk must propagate as Err, got {outcome:?}"
        );
    }

    // --- Issue #541: concurrent `git worktree add` reads a half-created
    // administrative directory ---

    /// A real git repo with one commit — `git worktree add` needs a commit to
    /// branch from. Disk-backed (issue #322 / CLAUDE.md rule 14): this fixture
    /// is a git repository plus its worktrees, not a scratch file.
    fn init_repo_with_commit(repo: &Path) {
        std::fs::create_dir_all(repo).expect("create repo dir");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "--initial-branch=main", "--quiet"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        // The dev box may have `commit.gpgsign` on globally; the fixture must
        // not depend on a signing key being present.
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "seed\n").expect("write seed file");
        git(&["add", "README.md"]);
        git(&["commit", "--quiet", "-m", "seed"]);
    }

    /// Stage the state a `git worktree add` leaves in
    /// `$GIT_COMMON_DIR/worktrees/<name>` *between* creating the entry and
    /// finishing it — the window issue #541 is about.
    ///
    /// The byte sequence is git's own, read off an `strace` of `git worktree
    /// add` (2.55.0), which in order: `mkdir(worktrees/<name>)`, writes
    /// `locked`, writes `gitdir`, then `openat("commondir", O_CREAT|O_TRUNC)`
    /// — and only on the NEXT syscall writes `../..` into it. Every other `git
    /// worktree add` on the repo scans the worktree list before creating its own
    /// entry and reads each entry's `commondir`; on a short read git calls
    /// `die_errno()`, which prints `strerror(errno)` for an errno that was never
    /// set. That is where the reported message's giveaway `: Success` comes from.
    ///
    /// Staged rather than raced because that window is two adjacent syscalls
    /// wide. It IS reachable by genuine concurrency — measured on this box with
    /// N concurrent real `git worktree add`s against one repo: 12 failures in 960
    /// adds at N=64, 7 in 1024 at N=128, 0 in 960 at N=3 and 0 in 400 at N=16,
    /// every failure carrying the reported `fatal: failed to read
    /// .git/worktrees/<name>/commondir: Success` verbatim. About a thousand real
    /// worktree checkouts per observed failure is neither affordable in the fast
    /// tier nor a reliable gate, so the test stages the identical bytes and
    /// closes the window on a timer instead of on luck.
    fn begin_half_created_entry(repo: &Path, name: &str) -> PathBuf {
        let entry = repo.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&entry).expect("create half-created worktree entry");
        std::fs::write(entry.join("locked"), "creating\n").expect("write locked");
        std::fs::write(
            entry.join("gitdir"),
            format!("{}\n", repo.join(name).join(".git").display()),
        )
        .expect("write gitdir");
        // The `O_CREAT|O_TRUNC` has happened; the write of `../..` has not.
        std::fs::write(entry.join("commondir"), b"").expect("create empty commondir");
        entry
    }

    /// The writer's very next syscall: `commondir` becomes readable and the
    /// window closes, exactly as it does when the concurrent add proceeds.
    fn finish_half_created_entry(entry: &Path) {
        std::fs::write(entry.join("commondir"), "../..\n").expect("finish commondir");
    }

    /// Issue #541 — three concurrent dispatches (`scheduler/dispatch/015`'s
    /// shape) must each end up with their worktree even though an unrelated
    /// `git worktree add` is mid-flight on the same repo. Each of the three
    /// scans the half-created entry, so each dies on it before its own worktree
    /// is created — the reported symptom, in the setup step, before any agent
    /// runs.
    ///
    /// The window is closed by a fourth party that holds no deck lock, so this
    /// exercises the case serialization alone cannot fix: a `git worktree add`
    /// the deck did not start (the user's own, or another tool's).
    ///
    /// Also pins the second-order defect: the add that dies on the scan has
    /// ALREADY created its `-b` branch, so a retry has to re-probe and ATTACH
    /// that branch rather than pass `-b` again — hence the per-dispatch HEAD
    /// assertion.
    #[tokio::test]
    async fn create_worktree_survives_a_concurrent_adds_half_created_entry() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        init_repo_with_commit(&repo);
        let entry = begin_half_created_entry(&repo, "concurrent-add");

        // Closes while the three dispatches are in flight. Not a timing race:
        // without a retry there is no second attempt for the timer to rescue,
        // so a slow machine cannot turn this green by accident.
        let closing = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            finish_half_created_entry(&entry);
        });

        let mut fires = Vec::new();
        for name in ["alpha", "beta", "gamma"] {
            let clone_dir = repo.clone();
            let worktree_dir = scratch.path().join(format!("repo-dispatch-{name}"));
            fires.push(tokio::spawn(async move {
                let branch = format!("agent/dispatch-{name}");
                let outcome = create_worktree(
                    &clone_dir,
                    &worktree_dir,
                    &branch,
                    false,
                    Creator::dispatch("unit"),
                )
                .await;
                (name, worktree_dir, branch, outcome)
            }));
        }
        closing.await.expect("window-closing task");

        for fire in fires {
            let (name, worktree_dir, branch, outcome) = fire.await.expect("dispatch task");
            assert_eq!(
                outcome,
                Ok(WorktreeCreation::Created),
                "dispatch '{name}' must get its worktree despite a concurrent add's \
                 half-created entry; `Err(… commondir …)` is issue #541 itself, and \
                 `Ok(BranchExists)`/`… already exists` is a retry that failed to \
                 re-probe the branch its own failed attempt left behind"
            );
            assert!(
                worktree_dir.join("README.md").exists(),
                "dispatch '{name}' reported Created but its worktree has no checkout at {}",
                worktree_dir.display()
            );
            let head = run_capture_args(
                "git",
                &[
                    "-C",
                    &worktree_dir.to_string_lossy(),
                    "branch",
                    "--show-current",
                ],
            )
            .await
            .expect("read the new worktree's branch");
            assert_eq!(
                head.trim(),
                branch,
                "dispatch '{name}' must be checked out on its own branch"
            );
        }
    }

    /// Control for the test above, and the guard on the retry's blast radius: a
    /// `commondir` that never becomes readable is NOT transient — a crashed add
    /// leaves exactly this behind — so it must still surface as `Err` naming the
    /// file, not be retried away or swallowed. Bounds the retry too: if it ever
    /// became unbounded this test would hang rather than fail.
    #[tokio::test]
    async fn create_worktree_surfaces_a_half_created_entry_that_never_completes() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        init_repo_with_commit(&repo);
        let _entry = begin_half_created_entry(&repo, "abandoned-add");
        let worktree_dir = scratch.path().join("repo-dispatch-stuck");

        let err = create_worktree(
            &repo,
            &worktree_dir,
            "agent/dispatch-stuck",
            false,
            Creator::dispatch("stuck"),
        )
        .await
        .expect_err("a permanently unreadable commondir must surface as an error");
        assert!(
            err.contains("commondir"),
            "the error must still name the file git could not read, got: {err}"
        );
        assert!(
            !worktree_dir.exists(),
            "a failed creation must not leave a worktree behind at {}",
            worktree_dir.display()
        );
    }

    /// Issue #541, the other half of the fix: worktree creation is serialized
    /// PER REPOSITORY and, because the lock is an `flock(2)` on a file (a named
    /// mutex on Windows), between separate deck processes rather than merely
    /// between tasks in one — concurrent dispatches can come from different
    /// decks, and an in-process mutex would silently not cover them.
    ///
    /// Written the way the platform lock's own contract test is: a held lock
    /// makes the creation block, and releasing it lets the creation complete.
    /// The lock is taken through the SAME derivation production uses, so a
    /// change that moved worktree creation off this key would fail here rather
    /// than quietly stop serializing.
    #[tokio::test]
    async fn create_worktree_serializes_per_repository_across_processes() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        init_repo_with_commit(&repo);

        let lock_path = worktree_lock_path(&repo)
            .await
            .expect("a git repo must resolve a worktree lock path");
        assert!(
            lock_path.starts_with(repo.join(".git")),
            "the lock must live in the repository's git common dir (the contended \
             directory), got {}",
            lock_path.display()
        );
        let held = crate::platform::lock::acquire_spawn_lock(&lock_path)
            .await
            .expect("hold the repository's worktree lock, as another deck process would");

        let clone_dir = repo.clone();
        let worktree_dir = scratch.path().join("repo-dispatch-serialized");
        let creating = tokio::spawn(async move {
            create_worktree(
                &clone_dir,
                &worktree_dir,
                "agent/dispatch-serialized",
                false,
                Creator::dispatch("serialized"),
            )
            .await
        });
        // Long enough to reach the blocking wait. Not a race: while the lock is
        // held the creation can NEVER finish, so a slow machine also passes.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !creating.is_finished(),
            "worktree creation must wait while another process holds this \
             repository's worktree lock; not waiting is how two `git worktree add`s \
             end up reading each other's half-created entries (#541)"
        );

        drop(held);

        let outcome = tokio::time::timeout(Duration::from_secs(30), creating)
            .await
            .expect("releasing the lock must let the creation proceed")
            .expect("the creating task must not panic");
        assert_eq!(
            outcome,
            Ok(WorktreeCreation::Created),
            "once the lock is released the worktree must be created normally"
        );
    }

    /// A dispatch whose name is claimed by a LIVE worktree must be told that,
    /// not that its branch is left over from a dispatch "whose worktree is
    /// already gone" — the user can see the directory, and the leftover-branch
    /// message sends them to `git branch -D` for a tree another dispatch is
    /// working in.
    ///
    /// The mirror image of `dispatch.rs`'s
    /// `second_dispatch_of_a_name_reports_branch_exists_after_cleanup`, which
    /// pins the same distinction from the other side (dir gone → BranchExists).
    /// Serializing creation (#541) is what makes this reachable by design rather
    /// than by luck: the loser of a same-name race now always probes the branch
    /// AFTER the winner created it.
    #[tokio::test]
    async fn create_worktree_reports_a_live_claim_not_a_leftover_branch() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        init_repo_with_commit(&repo);
        let worktree_dir = scratch.path().join("repo-dispatch-claimed");

        assert_eq!(
            create_worktree(
                &repo,
                &worktree_dir,
                "agent/dispatch-claimed",
                false,
                Creator::dispatch("claimed")
            )
            .await,
            Ok(WorktreeCreation::Created),
            "precondition: the first dispatch claims the name"
        );

        assert_eq!(
            create_worktree(
                &repo,
                &worktree_dir,
                "agent/dispatch-claimed",
                false,
                Creator::dispatch("claimed")
            )
            .await,
            Ok(WorktreeCreation::AlreadyClaimed),
            "a second dispatch of a name whose worktree is still THERE is a live \
             claim; reporting BranchExists would tell the user their worktree is \
             gone while it is in front of them"
        );
        assert!(
            worktree_dir.exists(),
            "the live claim must be left untouched at {}",
            worktree_dir.display()
        );
    }

    /// The retry predicate matches the reported signature — including the
    /// `: Success` tell (`strerror` on an errno nothing set) — and does not
    /// match ordinary `git worktree add` failures, which must stay hard
    /// failures rather than costing a dispatch 1.5s of pointless backoff.
    #[test]
    fn worktree_scan_short_read_matches_only_the_commondir_signature() {
        // Verbatim from the issue…
        assert!(is_worktree_scan_short_read(
            "fatal: failed to read '.git/worktrees/repo-dispatch-alpha/commondir': Success"
        ));
        // …and verbatim from a real concurrent reproduction on git 2.55.0,
        // which prints the path unquoted.
        assert!(is_worktree_scan_short_read(
            "`git -C /tmp/repo worktree add /tmp/wt -b agent/x` failed (exit status: 128): \
             Preparing worktree (new branch 'agent/x')\n\
             fatal: failed to read .git/worktrees/w-2-2/commondir: Success"
        ));

        for genuine in [
            "fatal: a branch named 'agent/dispatch-alpha' already exists",
            "fatal: '/ws/repo-dispatch-alpha' already exists",
            "fatal: invalid reference: agent/nope",
            "fatal: not a git repository (or any of the parent directories): .git",
            "error: could not create leading directories of '/ws/x': Permission denied",
        ] {
            assert!(
                !is_worktree_scan_short_read(genuine),
                "a genuine failure must not be retried: {genuine}"
            );
        }
    }

    // --- Issue #425: the ownership marker is written at creation time ---

    /// The marker `worktree_reclaim` reads must actually be written by the one
    /// code path that runs `git worktree add`, and it must land in the
    /// worktree's own git metadata dir rather than anywhere in the working
    /// tree. Both halves matter: a marker inside the tree makes
    /// `git status --porcelain` non-empty forever, and the reclaim gate keeps
    /// every dirty worktree — so an in-tree marker would make the worktree
    /// permanently UNreclaimable, defeating the feature it enables.
    #[tokio::test]
    async fn create_worktree_marks_the_worktree_as_deck_owned_without_dirtying_it() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        init_repo_with_commit(&repo);
        let worktree_dir = scratch.path().join("repo-dispatch-marked");

        assert_eq!(
            create_worktree(
                &repo,
                &worktree_dir,
                "agent/dispatch-marked",
                false,
                Creator::dispatch("marked"),
            )
            .await,
            Ok(WorktreeCreation::Created)
        );

        let marker = crate::worktree_owner::marker_path(&worktree_dir)
            .expect("the created worktree must have a resolvable git metadata dir");
        assert!(
            marker.is_file(),
            "the deck must claim the worktree it just created; no marker at {}",
            marker.display()
        );
        assert!(
            !marker.starts_with(&worktree_dir),
            "the marker must live in the worktree's git metadata dir, never inside the \
             working tree — got {}",
            marker.display()
        );

        let status = std::process::Command::new("git")
            .current_dir(&worktree_dir)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "marking a worktree must not make it dirty — a dirty worktree is kept by the \
             reclaim gate, so an in-tree marker would make marked worktrees permanently \
             unreclaimable; got:\n{}",
            String::from_utf8_lossy(&status.stdout)
        );

        // Idempotent: a re-created or re-attached worktree must not accumulate
        // state. Checked by parsing rather than by comparing bytes, because an
        // APPEND is exactly what would break — two concatenated documents are
        // not one document — while a legitimate rewrite changes the timestamp.
        crate::worktree_owner::write_marker(
            &worktree_dir,
            "agent/dispatch-marked",
            &Creator::dispatch("marked"),
        )
        .expect("re-marking an already-marked worktree must succeed");
        let after = std::fs::read_to_string(&marker).expect("read marker again");
        serde_json::from_str::<serde_json::Value>(&after).unwrap_or_else(|e| {
            panic!(
                "re-marking must REPLACE the marker, never append to it: after a second \
                 write the file must still be one document, but it did not parse ({e}):\n\
                 {after}"
            )
        });
    }

    /// The dangerous direction. `AlreadyClaimed` means the worktree DIRECTORY
    /// was already on disk when our `git worktree add` ran, so this process did
    /// not create it — and the marker is an ownership claim consumed by a path
    /// that DELETES directories. Claiming a directory we did not create is the
    /// one failure this marker exists to prevent, so the already-claimed arm
    /// must leave the marker alone. (A concurrent dispatch that genuinely
    /// created it writes its own marker from its own `Created` arm.)
    #[tokio::test]
    async fn create_worktree_never_marks_a_worktree_it_did_not_create() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("repo");
        init_repo_with_commit(&repo);
        let worktree_dir = scratch.path().join("repo-dispatch-foreign");

        // Somebody else's worktree, on this same repo, at the path our dispatch
        // is about to want: a real linked worktree, so it HAS a git metadata
        // dir a marker could be written into.
        let add = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-b", "someone-elses"])
            .arg(&worktree_dir)
            .output()
            .expect("git worktree add");
        assert!(
            add.status.success(),
            "fixture precondition: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        let marker = crate::worktree_owner::marker_path(&worktree_dir)
            .expect("the foreign worktree must have a resolvable git metadata dir");
        assert!(
            !marker.is_file(),
            "fixture precondition: a plain `git worktree add` leaves no marker"
        );

        assert_eq!(
            create_worktree(
                &repo,
                &worktree_dir,
                "agent/dispatch-foreign",
                false,
                Creator::dispatch("foreign"),
            )
            .await,
            Ok(WorktreeCreation::AlreadyClaimed),
            "precondition: a present worktree dir is reported as already claimed"
        );
        assert!(
            !marker.is_file(),
            "a worktree the deck did not create must never be marked as deck-owned — \
             the marker gates an unattended `git worktree remove`; found one at {}",
            marker.display()
        );
    }

    // --- .worktrees/ git-status hygiene via .git/info/exclude ---

    // PRD #120 — provisioning keeps `.worktrees/` out of the clone's `git status`
    // by appending it to the clone-LOCAL `.git/info/exclude` (never a committed
    // .gitignore — the clone is the user's). Idempotent: a second fire must not
    // duplicate the line.
    #[test]
    fn ensure_worktrees_excluded_appends_once_idempotently() {
        let clone = tempfile::tempdir().unwrap();
        // Initialize the clone with a `.git/info/` structure.
        let info_dir = clone.path().join(".git").join("info");
        std::fs::create_dir_all(&info_dir).unwrap();
        let exclude_path = info_dir.join("exclude");

        // First fire writes the `.worktrees/` exclude line.
        ensure_worktrees_excluded(clone.path());
        let after_first = std::fs::read_to_string(&exclude_path).unwrap();
        assert!(
            after_first.lines().any(|l| l.trim() == ".worktrees/"),
            ".git/info/exclude should contain the .worktrees/ line, got {after_first:?}"
        );

        // Second fire must NOT duplicate it.
        ensure_worktrees_excluded(clone.path());
        let after_second = std::fs::read_to_string(&exclude_path).unwrap();
        let count = after_second
            .lines()
            .filter(|l| l.trim() == ".worktrees/")
            .count();
        assert_eq!(
            count, 1,
            "repeated fires must not duplicate the exclude line, got {after_second:?}"
        );
    }

    // --- S5: workspace absolutization ---

    #[test]
    fn canonical_workspace_requires_absolute() {
        // Relative roots are rejected on every platform (bare/`.`-prefixed are
        // relative everywhere), so these assertions need no cfg gate.
        assert!(canonical_workspace("relative/dir").is_err());
        assert!(canonical_workspace("./also/relative").is_err());

        // The accepted-absolute fixture must be a *genuinely* absolute path on
        // the host: on Windows a POSIX-style "/work/space" is NOT absolute
        // (Path::is_absolute wants a drive/prefix like `C:\`), so pick the
        // literal by platform. Precedent: commit 8796fc3 made the config-path
        // tests platform-aware for the same build-windows CI job.
        #[cfg(windows)]
        let abs_root = r"C:\work\space";
        #[cfg(not(windows))]
        let abs_root = "/work/space";
        let abs = canonical_workspace(abs_root).expect("absolute path accepted");
        assert!(abs.is_absolute());
        assert_eq!(abs, PathBuf::from(abs_root));
    }
}
