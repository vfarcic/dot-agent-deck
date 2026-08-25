use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent_pty::AgentPtyRegistry;
use crate::event::BroadcastMsg;
use crate::issue_dispatch_run::{
    RemovalPolicy, WorktreeCreation, WorktreeRegistry, create_worktree, record_worktree,
    remove_worktree, run_capture_args, run_status,
};
use crate::scheduler::StderrNotifier;
use crate::spawn::{SpawnKind, SpawnRequest, SpawnShapeOverride, spawn};
use crate::worktree_owner::Creator;

/// PRD #220: the orchestrations a dispatch out of `dir` could start, by resolved
/// name. Empty means only a single agent is available.
///
/// `dir` must be the CALLER's repo dir — the same directory `handle_dispatch`
/// resolves its target from. An earlier cut computed this in the CLI process from
/// its own `current_dir()` and let the spawn resolve names against the WORKTREE
/// dir instead; because `load_project_config` normalises an unnamed orchestration
/// to its directory basename, the same entry was then `myrepo` in the listing and
/// `myrepo-dispatch-<slug>` at spawn time — a name the listing offered and the
/// spawn could never match. The listing is now answered by the daemon
/// ([`list_targets_response`]) precisely so both sides share one basis.
///
/// Roleless `[[orchestrations]]` are filtered out because the spawn skips them
/// too — listing one would offer a target that cannot be spawned.
pub fn available_orchestrations(
    config: Option<&crate::project_config::ProjectConfig>,
    dir: &Path,
) -> Vec<(String, usize)> {
    config
        .map(|cfg| {
            cfg.orchestrations
                .iter()
                .filter(|o| !o.roles.is_empty())
                .map(|o| {
                    (
                        crate::project_config::resolve_orchestration_name(&o.name, dir),
                        o.roles.len(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Human-readable `--list-targets` output, read by the dispatcher agent and
/// relayed to the user.
///
/// Schedule/authoring modes are absent by construction: a schedule creates a
/// FUTURE task, so it is not something a dispatch can start, and the dispatcher
/// option itself is not a target either. Only real spawn shapes appear.
pub fn render_available_targets(orchestrations: &[(String, usize)]) -> String {
    let mut out = String::from("Available dispatch targets:\n");
    out.push_str("  single            one agent (--single)\n");
    if orchestrations.is_empty() {
        out.push_str(
            "\nNo orchestrations are defined here, so `single` is the only target.\n\
             Dispatch with `--single`.\n",
        );
        return out;
    }
    for (name, roles) in orchestrations {
        // The name is SINGLE-QUOTED in the suggested command, not bare: an
        // orchestration named `code review` produced `--orchestration code review`,
        // which clap reads as the name `code` plus a stray positional and rejects
        // outright — leaving no way to pick the target just offered.
        out.push_str(&format!(
            "  orchestration     '{name}' — {roles} roles (--orchestration '{name}')\n"
        ));
    }
    out.push_str(
        "\nAsk the user which they want before dispatching, then pass the matching flag.\n",
    );
    out
}

/// Build the daemon's reply to a `--list-targets` request for `cwd`.
///
/// Four states the caller must be able to tell apart, none of which an empty list
/// alone can express:
///
/// * pane cwd UNKNOWN (no matching agent record) → say so. Rendering this as "no
///   orchestrations are defined here" would be a claim about a repo we never
///   looked at, and the agent would relay it as fact;
/// * no config file → only `single` is available, which is the truth;
/// * config present but UNPARSEABLE → `error` is set and named, because
///   `load_config_for_dir` swallows the parse error and a silent "no orchestrations
///   here" would walk the user past a broken config without ever learning it is
///   broken;
/// * config parsed → every role-bearing orchestration, under the name the spawn
///   will resolve it to.
pub fn list_targets_response(cwd: Option<&Path>) -> crate::event::ListTargetsResponse {
    use crate::event::{ListTargetsResponse, ListedOrchestration};
    let Some(dir) = cwd else {
        let msg = "could not determine this pane's working directory".to_string();
        return ListTargetsResponse {
            rendered: "Could not determine this pane's working directory, so the available \
                       orchestrations are unknown. This is NOT the same as the repo having \
                       none — do not report it that way. Dispatch `--single` to start one \
                       agent, or `--orchestration <name>` if you know the name.\n"
                .to_string(),
            orchestrations: Vec::new(),
            error: Some(msg),
        };
    };
    match crate::project_config::load_project_config(dir) {
        Ok(config) => {
            let found = available_orchestrations(config.as_ref(), dir);
            ListTargetsResponse {
                rendered: render_available_targets(&found),
                orchestrations: found
                    .into_iter()
                    .map(|(name, roles)| ListedOrchestration { name, roles })
                    .collect(),
                error: None,
            }
        }
        Err(e) => {
            let msg = format!("{e}");
            ListTargetsResponse {
                rendered: format!(
                    "Could not read this repo's .dot-agent-deck.toml, so the available \
                     orchestrations are unknown:\n  {msg}\n\nFix the config, or dispatch \
                     `--single` (which needs no config).\n"
                ),
                orchestrations: Vec::new(),
                error: Some(msg),
            }
        }
    }
}

/// The command a single-agent dispatch runs.
///
/// `SpawnRequest.command: None` means `$SHELL` in the spawn path, so passing None
/// here starts a **shell**, not an agent: the worktree appears, a pane appears,
/// and the `--task` prompt is typed into a bash prompt. Before the shape selector
/// this repo never took the single-agent branch (role commands win for an
/// orchestration), which is why it went unnoticed — but any repo with no
/// `[[orchestrations]]` already hit it.
///
/// So resolve a real agent command: the deck's configured `default_command` when
/// set, else the Claude default, mirroring what the interactive new-pane form does
/// for a blank Command field (`resolve_authoring_command`). "Single agent" has to
/// mean an agent.
pub fn resolve_single_agent_command(configured: Option<&str>) -> String {
    let trimmed = configured.unwrap_or_default().trim();
    if trimmed.is_empty() {
        crate::agent_registry::CLAUDE_CODE
            .default_command
            .unwrap_or("claude")
            .to_string()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_name(name: &str) -> String {
    let slug_chars: String = name
        .replace("..", "_")
        .replace('\0', "")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug_chars.is_empty() || slug_chars.chars().all(|c| c == '-') {
        "dispatch".to_string()
    } else {
        slug_chars.trim_matches('-').to_string()
    }
}

struct DispatchPaths {
    worktree_dir: PathBuf,
    branch: String,
}

/// Derive the sibling worktree dir + head branch for one dispatch.
///
/// Sibling layout (`../<repo>-dispatch-<slug>`) rather than nested inside the
/// caller's checkout: a nested tree would be walked by every `rg`, IDE index and
/// file watcher in the parent, and `git clean -xdff` would take it along with any
/// uncommitted agent work. This matches `/worktree-prd`'s `create.sh`.
///
/// `file_name()` is absent for a filesystem root (`/`) and for a path ending in
/// `..`; fall back to a fixed stem rather than panicking, since `working_dir`
/// comes from an agent record and a daemon must not die on a surprising cwd.
fn derive_dispatch_paths(working_dir: &Path, name: &str) -> DispatchPaths {
    let clean_name = sanitize_name(name);
    let slug = format!("dispatch-{clean_name}");
    let stem = working_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let worktree_dir = working_dir
        .parent()
        .unwrap_or(working_dir)
        .join(format!("{stem}-{slug}"));
    let branch = format!("agent/{slug}");
    DispatchPaths {
        worktree_dir,
        branch,
    }
}

/// Human-readable description of the commit a dispatch is about to cut its
/// worktree from — `"main at c701932"`, or `"detached HEAD at c701932"`.
///
/// Issue #674: `dispatch` takes no base or branch option. [`create_worktree`]
/// runs `git worktree add <dir> -b <branch>` with no start-point, which git
/// resolves to the CALLER's `HEAD`, so the freshness of the caller's checkout is
/// the only thing deciding where a unit starts — and nothing in the dispatch
/// output named it. A stale or simply unexpected base (a feature branch the user
/// happened to be standing on) was therefore invisible at the one moment the
/// user is looking: three units cut from a `main` six commits behind
/// `origin/main` reported exactly the same success line as three cut from a
/// current one.
///
/// Deliberately best-effort and `Option`: this is a reporting nicety on a path
/// that has already done the real work, so a probe that fails must drop the
/// clause rather than fail the dispatch. Read BEFORE the worktree is created,
/// which is the state `git worktree add` will actually resolve.
async fn describe_dispatch_base(clone_dir: &Path) -> Option<String> {
    let clone = clone_dir.to_string_lossy();
    let head = run_capture_args("git", &["-C", &clone, "rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .ok()?;
    let sha = run_capture_args("git", &["-C", &clone, "rev-parse", "--short", "HEAD"])
        .await
        .ok()?;
    let (head, sha) = (head.trim(), sha.trim());
    if head.is_empty() || sha.is_empty() {
        return None;
    }
    // `rev-parse --abbrev-ref HEAD` answers the literal string `HEAD` when the
    // checkout is detached, which as a branch name would read as a lie.
    if head == "HEAD" {
        return Some(format!("detached HEAD at {sha}"));
    }
    Some(format!("{head} at {sha}"))
}

pub struct DispatchResult {
    pub worktree_dir: PathBuf,
    pub success: bool,
    pub message: String,
}

pub struct DispatchContext {
    pub working_dir: PathBuf,
    pub registry: Arc<AgentPtyRegistry>,
    pub event_tx: tokio::sync::broadcast::Sender<BroadcastMsg>,
    /// The daemon-wide worktree registry the tab-close handler reads. Uses the
    /// [`WorktreeRegistry`] alias rather than spelling the map out, so the entry
    /// type cannot drift away from the registry it has to interoperate with.
    pub worktrees: WorktreeRegistry,
    /// The deck's configured `default_command`, resolved by the caller (mirroring
    /// the issue-dispatch precedent in `daemon.rs`). Used ONLY when the dispatch
    /// starts a single agent — an orchestration's role commands win. Passed in
    /// rather than read here so [`handle_dispatch`] does not depend on global
    /// config. See [`resolve_single_agent_command`].
    pub default_command: Option<String>,
    /// The daemon's [`AppState`](crate::state::AppState), so a dispatched
    /// ORCHESTRATION's role panes are registered in the maps `handle_delegate`
    /// routes on. Without it the dispatch produces an orchestrator that has been
    /// handed a delegation protocol it cannot use — see
    /// [`crate::state::AppState::register_orchestration_role`]. `None` in unit
    /// tests, which assert on the worktree/spawn result rather than on routing.
    pub state: Option<crate::state::SharedState>,
}

/// Translate the wire choice into the spawn-side override.
///
/// `None` on the wire means "whatever the dispatched worktree's config implies",
/// which is [`SpawnShapeOverride`]-absent — i.e. exactly the pre-selector
/// behaviour, so an older CLI keeps working against a newer daemon.
fn shape_override_of(shape: Option<&crate::event::DispatchShape>) -> Option<SpawnShapeOverride> {
    match shape {
        None => None,
        Some(crate::event::DispatchShape::SingleAgent) => Some(SpawnShapeOverride::SingleAgent),
        Some(crate::event::DispatchShape::Orchestration { name }) => {
            Some(SpawnShapeOverride::Orchestration(name.clone()))
        }
    }
}

pub async fn handle_dispatch(
    ctx: &DispatchContext,
    name: &str,
    task: &str,
    shape: Option<&crate::event::DispatchShape>,
) -> DispatchResult {
    let paths = derive_dispatch_paths(&ctx.working_dir, name);
    let clone_dir = ctx.working_dir.clone();

    // Resolve the shape from the CALLER's repo config, BEFORE any git work.
    //
    // Caller-side because that is the config the user chose from: the worktree is a
    // HEAD checkout (uncommitted config invisible) and `load_project_config`
    // normalises an unnamed orchestration to its directory basename, so the same
    // entry is `myrepo` here and `myrepo-dispatch-<slug>` there.
    //
    // Before the worktree because a rejected shape must not leave debris: validating
    // inside `spawn` meant a typo'd `--orchestration` created a worktree and branch,
    // rolled them back, and reported "failed to spawn agent" for what is a plain
    // validation error.
    let single_command = resolve_single_agent_command(ctx.default_command.as_deref());
    let resolved_target = match crate::spawn::decide_target_with_override(
        crate::spawn::load_config_for_dir(&clone_dir).as_ref(),
        &clone_dir,
        Some(single_command.as_str()),
        shape_override_of(shape).as_ref(),
    ) {
        Ok(t) => t,
        Err(e) => {
            return DispatchResult {
                worktree_dir: paths.worktree_dir.clone(),
                success: false,
                message: format!("dispatch: {e}"),
            };
        }
    };

    // Captured before the worktree exists, so it names the state `git worktree
    // add` resolves its (absent) start-point against. See
    // [`describe_dispatch_base`] for why this is reported at all.
    let base = describe_dispatch_base(&clone_dir).await;

    match create_worktree(
        &clone_dir,
        &paths.worktree_dir,
        &paths.branch,
        false,
        Creator::dispatch(name),
    )
    .await
    {
        Ok(WorktreeCreation::Created) => {}
        Ok(WorktreeCreation::AlreadyClaimed) => {
            return DispatchResult {
                worktree_dir: paths.worktree_dir.clone(),
                success: false,
                message: format!(
                    "dispatch: worktree {} is already claimed by another dispatch. \
                     Wait for it to finish, or dispatch under a different name.",
                    paths.worktree_dir.display()
                ),
            };
        }
        // The worktree dir is GONE but its branch survived — `git worktree
        // remove` never deletes the branch, so this is the ordinary state after a
        // previous dispatch of the same name was cleaned up. Say so, and name
        // both fixes: the branch is not deleted implicitly because it may hold
        // that dispatch's committed work.
        Ok(WorktreeCreation::BranchExists) => {
            return DispatchResult {
                worktree_dir: paths.worktree_dir.clone(),
                success: false,
                message: format!(
                    "dispatch: branch {branch} already exists from an earlier dispatch named \
                     '{name}' (its worktree is already gone). That branch may hold committed \
                     work, so it is left alone. Dispatch under a different name, or run \
                     `git -C {clone} branch -D {branch}` first if you are done with it.",
                    branch = paths.branch,
                    name = name,
                    clone = clone_dir.display(),
                ),
            };
        }
        Err(e) => {
            return DispatchResult {
                worktree_dir: paths.worktree_dir.clone(),
                success: false,
                message: format!("dispatch: failed to create worktree: {e}"),
            };
        }
    }

    // `RemovalPolicy::KeepIfDirty`: this worktree is a sibling of the user's own
    // checkout and its name was chosen by an LLM, so closing the tab must not
    // destroy uncommitted work. See [`RemovalPolicy`].
    record_worktree(
        &ctx.worktrees,
        &paths.worktree_dir,
        &clone_dir,
        RemovalPolicy::KeepIfDirty,
    );

    let prompt = task.to_string();

    let req = SpawnRequest {
        task_name: format!("dispatch-{name}"),
        working_dir: paths.worktree_dir.to_string_lossy().into_owned(),
        // A real agent command, never `None` — see `resolve_single_agent_command`.
        // Ignored when the dispatch starts an orchestration (role commands win).
        command: Some(single_command),
        prompt,
        resolved_target: Some(resolved_target),
        // PRD #222 parity, dispatch-only for now — see the field's docs.
        compose_orchestrator_context: true,
    };

    let notifier = StderrNotifier;

    match spawn(
        req,
        &ctx.registry,
        &notifier,
        Some(&ctx.event_tx),
        false,
        ctx.state.as_ref(),
    )
    .await
    {
        Ok(handle) => DispatchResult {
            worktree_dir: paths.worktree_dir.clone(),
            success: true,
            // Report what was ACTUALLY opened, from the spawn's own verdict.
            // `spawn` → `decide_target` branches on the dispatched worktree's
            // `.dot-agent-deck.toml`: a repo defining `[[orchestrations]]` gets a
            // full multi-role orchestration, anything else a single agent (PRD
            // #220 M1.1). Hardcoding either word makes this message a lie in the
            // other case — and it is written straight into the caller's pane, so
            // the dispatching agent repeats it to the user verbatim.
            //
            // The base clause is appended rather than interpolated into both
            // arms so a failed probe degrades to the pre-#674 sentence exactly,
            // instead of leaving a dangling "cut from ".
            message: {
                let opened = match &handle.kind {
                    SpawnKind::Orchestration { name: orch } => format!(
                        "dispatch: spawned isolated orchestration '{orch}' for '{name}' in {}",
                        paths.worktree_dir.display()
                    ),
                    SpawnKind::SingleAgent => format!(
                        "dispatch: spawned isolated agent for '{name}' in {}",
                        paths.worktree_dir.display()
                    ),
                };
                match &base {
                    Some(base) => format!("{opened}, cut from {base}"),
                    None => opened,
                }
            },
        },
        Err(e) => {
            let cleanup_note = match rollback_dispatched_worktree(
                &ctx.registry,
                &ctx.worktrees,
                &paths.worktree_dir,
                &clone_dir,
                &paths.branch,
            )
            .await
            {
                RollbackOutcome::Reclaimed {
                    branch_cleanup_failed: false,
                } => String::new(),
                RollbackOutcome::Reclaimed {
                    branch_cleanup_failed: true,
                } => " (cleanup failed: branch may still exist — name may be wedged)".to_string(),
                RollbackOutcome::Retained { live } => format!(
                    " (the worktree {wt} was LEFT IN PLACE: {live} agent(s) are still running \
                     in it, and removing it would delete their working directory underneath \
                     them. Close them to release it. Branch {branch} is retained too, so this \
                     dispatch name stays wedged until then.)",
                    wt = paths.worktree_dir.display(),
                    branch = paths.branch,
                ),
            };

            DispatchResult {
                worktree_dir: paths.worktree_dir,
                success: false,
                message: format!("dispatch: spawn failed: {e}{cleanup_note}"),
            }
        }
    }
}

/// What [`rollback_dispatched_worktree`] did.
#[derive(Debug, PartialEq, Eq)]
enum RollbackOutcome {
    /// Nothing was rooted in the tree, so the worktree and its branch are gone
    /// and the dispatch name is free again. `branch_cleanup_failed` is the
    /// pre-existing "the dir went but `git branch -D` did not" case.
    Reclaimed { branch_cleanup_failed: bool },
    /// `live` agents are still rooted in the tree, so it was left in place along
    /// with its branch and its worktree-registry entry.
    Retained { live: usize },
}

/// Undo a dispatch whose spawn failed — but never at the cost of deleting a
/// directory that live agents are working in.
///
/// **Issue #575.** This used to be an unconditional
/// `remove_worktree(.., RemovalPolicy::Force)` justified by the claim that "the
/// agent never started, so there is no user work to protect". That claim is true
/// for a single-agent dispatch and FALSE for a multi-role orchestration: `spawn`
/// launches roles in a loop, so a failure at role 2 leaves roles 0 and 1 as live
/// PTY children whose cwd is exactly this tree, and the force removal deleted it
/// out from under them. Issue #600's teardown in
/// [`crate::spawn`] now makes that the unreachable case rather than the ordinary
/// one — `spawn` returns `Err` only after tearing its partial roles down — but the
/// guard stays, because the invariant this function needs ("nothing is rooted
/// here") is cheap to verify and catastrophic to assume. A role whose child
/// somehow outlived its teardown must cost a retained directory, not a deleted
/// working directory.
///
/// The guard is
/// [`agents_rooted_in_worktree`](crate::issue_dispatch_run::agents_rooted_in_worktree),
/// the counting form of the
/// [`worktree_still_in_use`](crate::issue_dispatch_run::worktree_still_in_use)
/// predicate the tab-close path in `daemon_protocol` asks before its own removal
/// — one is defined in terms of the other, so the two call sites cannot drift on
/// what "rooted in" means, and the count is what lets the message name how many
/// agents the user has to close. Note the hazard here differs in kind from #236's (the
/// tab-close path's "never force-remove a dirty tree"): this is about live
/// processes, not uncommitted content, which is why `Force` is still correct once
/// the tree is genuinely empty of agents.
///
/// On `Retained` NOTHING is cleaned up — not the tree, not the branch, and not the
/// worktree-registry entry. Dropping the entry would be the worse half of the bug
/// it replaces: the surviving agents' eventual tab close is what triggers cleanup,
/// and it finds the tree by that entry, so removing it here would strand the
/// directory forever. Retaining the branch follows for free — `git branch -D`
/// refuses a branch that is checked out in a live worktree anyway.
async fn rollback_dispatched_worktree(
    registry: &Arc<AgentPtyRegistry>,
    worktrees: &WorktreeRegistry,
    worktree_dir: &Path,
    clone_dir: &Path,
    branch: &str,
) -> RollbackOutcome {
    let live = crate::issue_dispatch_run::agents_rooted_in_worktree(
        &registry.agent_records(),
        worktree_dir,
    );
    if live > 0 {
        tracing::warn!(
            worktree = %worktree_dir.display(),
            live,
            branch = %branch,
            "dispatch rollback: agents are still rooted in this worktree; leaving it \
             (and its branch) in place rather than deleting their working directory"
        );
        return RollbackOutcome::Retained { live };
    }

    // `Force` is correct now that the guard above has established nothing is
    // rooted here: this worktree was created seconds ago for an agent that is not
    // running, so there is no user work to protect — and it MUST actually go, or
    // the leftover dir and branch wedge this name for every later dispatch.
    remove_worktree(worktree_dir, clone_dir, RemovalPolicy::Force).await;
    // Also delete the branch: `git worktree remove` never deletes it, but on this
    // rollback path no agent is running so there is no committed work to protect —
    // leaving the branch would wedge this name for every later dispatch.
    let branch_cleanup_failed = run_status(
        "git",
        &["-C", &clone_dir.to_string_lossy(), "branch", "-D", branch],
    )
    .await
    .is_err();

    if branch_cleanup_failed {
        tracing::warn!(
            branch = %branch,
            "spawn rollback: failed to delete branch — name may be wedged for future dispatches"
        );
    }

    worktrees
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(worktree_dir);

    RollbackOutcome::Reclaimed {
        branch_cleanup_failed,
    }
}

// Issue #322: every scratch dir here goes through `crate::test_temp::tempdir()`
// rather than a bare `tempfile::tempdir()`. These tests build real git repos and
// real worktrees — the e2e-gated one below was measured holding a live 184 KiB
// `/tmp/.tmpYN3lNF` during a recorded `cargo test-e2e` — and the lib target does
// not link `tests/common/`, so nothing else moves them off the RAM-backed `/tmp`.
// `linkage-check` rule 8 covers this file, so a bare constructor cannot come back.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue_dispatch_run::{new_worktree_registry, take_worktree};

    /// Build a real git repo with one commit, so the `git worktree` primitives
    /// under test operate on a genuine repo rather than a stubbed one.
    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git available");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        std::fs::create_dir_all(dir).unwrap();
        run(&["init", "-q", "."]);
        run(&["config", "user.email", "t@t.t"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(dir.join("a.txt"), "hi").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        std::process::Command::new("git")
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(repo)
            .output()
            .expect("git available")
            .status
            .success()
    }

    /// Run a git command in `repo`, asserting it succeeded.
    fn git_in(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    }

    // --- the base a dispatch reports (issue #674) ---

    /// Issue #674: `dispatch` cuts every unit from the caller's `HEAD` and used
    /// to say nothing about it, so a unit started from a stale or unexpected
    /// branch produced exactly the same success line as one started from a
    /// current one. The reported base must name the branch the caller was
    /// standing on and the commit it resolved to.
    ///
    /// The branch is created explicitly rather than relying on whatever `git
    /// init` produced: `init.defaultBranch` is ambient user configuration, so
    /// asserting on `main` would pass or fail with the machine.
    #[tokio::test]
    async fn dispatch_base_names_the_branch_and_commit_it_was_cut_from() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        git_in(&repo, &["checkout", "-q", "-b", "feature-x"]);

        let base = describe_dispatch_base(&repo)
            .await
            .expect("a healthy repo must yield a base");

        let sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .current_dir(&repo)
                .output()
                .expect("git available")
                .stdout,
        )
        .unwrap();
        assert_eq!(base, format!("feature-x at {}", sha.trim()));
    }

    /// A detached checkout answers the literal string `HEAD` to `rev-parse
    /// --abbrev-ref`, which would read as a branch named "HEAD" — a lie in the
    /// one message the user relies on to know where their unit started.
    #[tokio::test]
    async fn dispatch_base_says_detached_rather_than_naming_a_branch_head() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        git_in(&repo, &["checkout", "-q", "--detach", "HEAD"]);

        let base = describe_dispatch_base(&repo)
            .await
            .expect("a detached checkout is still a healthy repo");

        assert!(
            base.starts_with("detached HEAD at "),
            "a detached checkout must not be reported as a branch, got {base:?}"
        );
    }

    /// The base is a reporting nicety on a path that has already created the
    /// worktree and spawned the agent, so a probe that cannot answer must drop
    /// the clause — never fail the dispatch, and never emit a dangling
    /// "cut from ".
    #[tokio::test]
    async fn dispatch_base_is_absent_rather_than_empty_outside_a_repo() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let not_a_repo = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&not_a_repo).unwrap();

        assert_eq!(describe_dispatch_base(&not_a_repo).await, None);
    }

    // --- slug + path derivation ---

    #[test]
    fn sanitize_name_neutralizes_path_traversal_and_separators() {
        // `..` and `/` must never survive into a path segment.
        assert!(!sanitize_name("../../etc/passwd").contains(".."));
        assert!(!sanitize_name("../../etc/passwd").contains('/'));
        // An all-punctuation name still yields a usable slug.
        assert_eq!(sanitize_name("///"), "dispatch");
        assert_eq!(sanitize_name(""), "dispatch");
        // Ordinary LLM-chosen slugs pass through untouched.
        assert_eq!(sanitize_name("fix-auth-bug"), "fix-auth-bug");
        assert_eq!(sanitize_name("add_rate_limiter"), "add_rate_limiter");
    }

    #[test]
    fn derive_dispatch_paths_places_worktree_as_sibling_not_nested() {
        let paths = derive_dispatch_paths(Path::new("/home/u/myrepo"), "fix-auth");
        assert_eq!(
            paths.worktree_dir,
            PathBuf::from("/home/u/myrepo-dispatch-fix-auth"),
            "the worktree must be a SIBLING of the checkout, never nested inside it"
        );
        assert_eq!(paths.branch, "agent/dispatch-fix-auth");
    }

    #[test]
    fn derive_dispatch_paths_survives_a_root_working_dir() {
        // `/` has no `file_name()`. This must not panic — it runs inside the
        // daemon's hook loop, where a panic kills the connection task.
        let paths = derive_dispatch_paths(Path::new("/"), "x");
        assert_eq!(paths.branch, "agent/dispatch-x");
        assert!(paths.worktree_dir.to_string_lossy().contains("dispatch-x"));
    }

    // --- the leftover-branch refusal (the one-shot-per-name defect) ---

    /// A dispatch name is reusable across cleanup cycles *as a diagnosable
    /// state*: `git worktree remove` PRESERVES the branch, so the second
    /// dispatch of a name must report `BranchExists` — NOT `AlreadyClaimed`,
    /// which would blame a worktree the user can see is already gone.
    #[tokio::test]
    async fn second_dispatch_of_a_name_reports_branch_exists_after_cleanup() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let paths = derive_dispatch_paths(&repo, "fix-auth");

        // First dispatch claims the name.
        assert_eq!(
            create_worktree(
                &repo,
                &paths.worktree_dir,
                &paths.branch,
                false,
                Creator::dispatch("fix-auth")
            )
            .await,
            Ok(WorktreeCreation::Created)
        );

        // Tab close: the worktree goes away, the branch does not.
        remove_worktree(&paths.worktree_dir, &repo, RemovalPolicy::KeepIfDirty).await;
        assert!(!paths.worktree_dir.exists(), "worktree dir should be gone");
        assert!(
            branch_exists(&repo, &paths.branch),
            "git worktree remove must not delete the branch — the premise of this test"
        );

        // Second dispatch of the SAME name: refused, but for the real reason.
        assert_eq!(
            create_worktree(
                &repo,
                &paths.worktree_dir,
                &paths.branch,
                false,
                Creator::dispatch("fix-auth")
            )
            .await,
            Ok(WorktreeCreation::BranchExists),
            "a leftover branch must be distinguishable from a claimed worktree"
        );
    }

    /// Deleting the leftover branch makes the name usable again — the recovery
    /// path the refusal message tells the user about.
    #[tokio::test]
    async fn deleting_the_leftover_branch_frees_the_name() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let paths = derive_dispatch_paths(&repo, "fix-auth");

        create_worktree(
            &repo,
            &paths.worktree_dir,
            &paths.branch,
            false,
            Creator::dispatch("fix-auth"),
        )
        .await
        .unwrap();
        remove_worktree(&paths.worktree_dir, &repo, RemovalPolicy::KeepIfDirty).await;
        std::process::Command::new("git")
            .args(["branch", "-D", &paths.branch])
            .current_dir(&repo)
            .output()
            .expect("git available");

        assert_eq!(
            create_worktree(
                &repo,
                &paths.worktree_dir,
                &paths.branch,
                false,
                Creator::dispatch("fix-auth")
            )
            .await,
            Ok(WorktreeCreation::Created),
            "after deleting the branch the same dispatch name must work again"
        );
    }

    // --- removal policy (the PRD #120 regression) ---

    /// `KeepIfDirty` (PRD #220 dispatch): uncommitted work in the worktree wins
    /// over cleanup — the tree stays so the user can recover it.
    #[tokio::test]
    async fn keep_if_dirty_preserves_a_worktree_with_uncommitted_work() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let paths = derive_dispatch_paths(&repo, "unit");
        create_worktree(
            &repo,
            &paths.worktree_dir,
            &paths.branch,
            false,
            Creator::dispatch("unit"),
        )
        .await
        .unwrap();
        std::fs::write(paths.worktree_dir.join("uncommitted.txt"), "work").unwrap();

        remove_worktree(&paths.worktree_dir, &repo, RemovalPolicy::KeepIfDirty).await;

        assert!(
            paths.worktree_dir.exists(),
            "a dirty dispatch worktree must survive tab close so work is recoverable"
        );
    }

    /// `Force` (PRD #120 issue-dispatch): the directory MUST go even when dirty,
    /// because `dispatch_decision` reads a surviving worktree as "issue already
    /// claimed" and would skip that issue on every later fire, permanently.
    /// This is the exact regression that dropping `--force` introduced.
    #[tokio::test]
    async fn force_removes_a_dirty_worktree_so_the_slot_is_reclaimable() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let worktree_dir = repo.join(".worktrees").join("issue-7");
        create_worktree(
            &repo,
            &worktree_dir,
            "agent/issue-7",
            true,
            Creator::issue_dispatch("unit", 7),
        )
        .await
        .unwrap();
        std::fs::write(worktree_dir.join("uncommitted.txt"), "wip").unwrap();

        remove_worktree(&worktree_dir, &repo, RemovalPolicy::Force).await;

        assert!(
            !worktree_dir.exists(),
            "issue-dispatch must force-remove so the vacated slot is reclaimable"
        );
    }

    // --- the policy survives the registry round-trip the close handler uses ---

    /// The close handler in `daemon_protocol.rs` sees only a path, so the policy
    /// has to come back out of the registry intact — otherwise both producers
    /// silently share whichever policy is hardcoded there.
    #[test]
    fn registry_round_trip_preserves_each_producers_policy() {
        let reg = new_worktree_registry();
        let clone = PathBuf::from("/ws/clone");
        let issue_wt = PathBuf::from("/ws/clone/.worktrees/issue-7");
        let dispatch_wt = PathBuf::from("/ws/clone-dispatch-fix-auth");

        record_worktree(&reg, &issue_wt, &clone, RemovalPolicy::Force);
        record_worktree(&reg, &dispatch_wt, &clone, RemovalPolicy::KeepIfDirty);

        assert_eq!(
            take_worktree(&reg, &issue_wt).map(|e| e.policy),
            Some(RemovalPolicy::Force)
        );
        assert_eq!(
            take_worktree(&reg, &dispatch_wt).map(|e| e.policy),
            Some(RemovalPolicy::KeepIfDirty)
        );
    }

    // --- PRD #220: the target listing + the wire choice ---

    fn cfg(toml: &str) -> crate::project_config::ProjectConfig {
        toml::from_str(toml).expect("parse project config")
    }

    /// The listing offers `single` always, plus every ROLE-BEARING orchestration
    /// by resolved name. Schedule/authoring modes never appear — they create a
    /// future task rather than starting a line of work, so they are not targets.
    #[test]
    fn available_targets_list_single_plus_every_role_bearing_orchestration() {
        let c = cfg("[[modes]]\nname = \"dev\"\n\n\
             [[orchestrations]]\nname = \"digest\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"sh\"\n\n\
             [[orchestrations]]\nname = \"review\"\n\n\
             [[orchestrations.roles]]\nname = \"lead\"\ncommand = \"cat\"\nstart = true\n");
        let found = available_orchestrations(Some(&c), Path::new("/tmp/repo"));
        assert_eq!(
            found,
            vec![("digest".to_string(), 2), ("review".to_string(), 1)]
        );

        let rendered = render_available_targets(&found);
        assert!(rendered.contains("--single"), "single is always offered");
        assert!(
            rendered.contains("--orchestration 'digest'"),
            "the name must be single-quoted so a name with spaces still parses:\n{rendered}"
        );
        assert!(rendered.contains("--orchestration 'review'"));
        assert!(
            !rendered.contains("schedule") && !rendered.contains("dev"),
            "modes and schedule authoring are not dispatch targets:\n{rendered}"
        );
    }

    /// An unnamed orchestration is listed under the name it will actually spawn
    /// as — the dir basename — so the name the agent passes back matches.
    #[test]
    fn available_targets_resolve_an_unnamed_orchestration_to_the_dir_basename() {
        let c = cfg("[[orchestrations]]\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n");
        let found = available_orchestrations(Some(&c), Path::new("/home/u/morning-digest"));
        assert_eq!(found, vec![("morning-digest".to_string(), 1)]);
    }

    /// No config at all: only `single`, and the text says so rather than leaving
    /// the agent to infer it from an empty list.
    #[test]
    fn available_targets_without_config_offer_single_only() {
        let found = available_orchestrations(None, Path::new("/tmp/repo"));
        assert!(found.is_empty());
        let rendered = render_available_targets(&found);
        assert!(rendered.contains("--single"));
        assert!(
            rendered.contains("No orchestrations are defined"),
            "the empty case must state the situation:\n{rendered}"
        );
    }

    /// A single-agent dispatch must run an AGENT, never `$SHELL`.
    ///
    /// `SpawnRequest.command: None` means `$SHELL` in the spawn path, so the
    /// original `None` started a shell and typed the `--task` prompt into a bash
    /// prompt. Reported from real use once `--single` made that branch reachable in
    /// a repo that defines `[[orchestrations]]`; it was already reachable in any
    /// repo without them.
    #[test]
    fn single_agent_dispatch_resolves_an_agent_command_never_a_shell() {
        // Configured command wins, whitespace-trimmed.
        assert_eq!(resolve_single_agent_command(Some("opencode")), "opencode");
        assert_eq!(resolve_single_agent_command(Some("  claude  ")), "claude");

        // Unset / blank falls back to a real agent, NOT an empty string (which the
        // spawn path would read as `$SHELL`).
        for blank in [None, Some(""), Some("   ")] {
            let resolved = resolve_single_agent_command(blank);
            assert!(
                !resolved.trim().is_empty(),
                "a blank default_command must still resolve to an agent, got {resolved:?}"
            );
            assert_eq!(
                resolved,
                crate::agent_registry::CLAUDE_CODE
                    .default_command
                    .unwrap_or("claude"),
                "the fallback must match what the new-pane form uses for a blank Command"
            );
        }
    }

    /// The listing must distinguish "unknown pane", "broken config" and "genuinely
    /// none". Collapsing any of them into the empty listing makes the agent report a
    /// claim about a repo nobody looked at — the same dishonesty as reading a parse
    /// error as "no orchestrations".
    #[test]
    fn list_targets_distinguishes_unknown_pane_broken_config_and_genuinely_none() {
        // Unknown pane: explicit, and NOT phrased as "no orchestrations".
        let unknown = list_targets_response(None);
        assert!(
            unknown.error.is_some(),
            "unknown cwd must be an error state"
        );
        assert!(unknown.orchestrations.is_empty());
        assert!(
            !unknown
                .rendered
                .contains("No orchestrations are defined here"),
            "must not claim the repo has none:\n{}",
            unknown.rendered
        );

        // Genuinely none: no config file at all.
        let tmp = crate::test_temp::tempdir().unwrap();
        let none = list_targets_response(Some(tmp.path()));
        assert!(none.error.is_none(), "an absent config is not an error");
        assert!(none.rendered.contains("No orchestrations are defined here"));

        // Broken config: named, and flagged as an error.
        let bad = crate::test_temp::tempdir().unwrap();
        std::fs::write(
            bad.path().join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"unterminated\n",
        )
        .unwrap();
        let broken = list_targets_response(Some(bad.path()));
        assert!(
            broken.error.is_some(),
            "an unparseable config must not read as 'no orchestrations':\n{}",
            broken.rendered
        );
        assert!(broken.rendered.contains(".dot-agent-deck.toml"));

        // Present and parseable: listed structurally as well as rendered.
        let good = crate::test_temp::tempdir().unwrap();
        std::fs::write(
            good.path().join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"digest\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"sh\"\n",
        )
        .unwrap();
        let ok = list_targets_response(Some(good.path()));
        assert!(ok.error.is_none());
        assert_eq!(ok.orchestrations.len(), 1);
        assert_eq!(ok.orchestrations[0].name, "digest");
        assert_eq!(ok.orchestrations[0].roles, 2);
    }

    /// An ORCHESTRATION dispatch must start the team WITH its delegation protocol.
    ///
    /// This is the defect reported from real use: the orchestration came up, its
    /// orchestrator received the task, and every worker sat idle — because the daemon
    /// spawn path never composed the orchestrator context that the interactive
    /// `Ctrl+n` path writes, so the orchestrator was never told it was one or how to
    /// `delegate`. Asserted on the CONTEXT FILE in the dispatched worktree, which is
    /// the artefact that was missing entirely.
    ///
    /// Roles run `cat` (alive on stdin, no LLM tokens), mirroring the `orch-deck`
    /// fixture.
    // Gated to the e2e tier: this spawns REAL PTYs and awaits the prompt-delivery
    // readiness gate, so it costs ~30s — too slow for the per-task fast gate, and
    // not a unit test by any honest reading.
    #[cfg(feature = "e2e")]
    #[tokio::test]
    async fn an_orchestration_dispatch_writes_the_delegation_protocol_and_the_task() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        std::fs::write(
            repo.join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"demo-orch\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"cat\"\ndescription = \"Does the work\"\n",
        )
        .unwrap();
        // The config must be COMMITTED: the shape is resolved from the caller's repo,
        // but the worktree the roles run in is a HEAD checkout.
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git available");
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", "add orchestration"]);

        let (event_tx, _rx) = tokio::sync::broadcast::channel(64);
        let ctx = DispatchContext {
            working_dir: repo.clone(),
            registry: Arc::new(AgentPtyRegistry::new()),
            event_tx,
            worktrees: new_worktree_registry(),
            default_command: None,
            // These unit tests assert on the worktree + spawn shape, not on
            // delegate routing (`orchestration/dispatch/001` owns that).
            state: None,
        };

        let result = handle_dispatch(
            &ctx,
            "team-unit",
            "Verify PR #232 and report back.",
            Some(&crate::event::DispatchShape::Orchestration { name: None }),
        )
        .await;

        let worktree = result.worktree_dir.clone();
        // Reclaim the sibling worktree regardless of the assertions below.
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(worktree.clone());

        assert!(
            result.success,
            "the orchestration dispatch should succeed, got: {}",
            result.message
        );
        assert!(
            result.message.contains("orchestration"),
            "the reported shape must say orchestration, got: {}",
            result.message
        );

        let context = worktree.join(".dot-agent-deck/orchestrator-context.md");
        let content = std::fs::read_to_string(&context).unwrap_or_else(|e| {
            panic!(
                "the dispatched orchestration must get an orchestrator-context.md at {} \
                 (its absence is exactly why workers sat idle): {e}",
                context.display()
            )
        });
        assert!(
            content.contains("Delegation protocol"),
            "the orchestrator must be told HOW to delegate:\n{content}"
        );
        assert!(
            content.contains("worker") && content.contains("Does the work"),
            "the orchestrator must be told WHICH agents exist:\n{content}"
        );
        assert!(
            content.contains("## Your task") && content.contains("Verify PR #232"),
            "the caller's task must ride inside the context file:\n{content}"
        );
    }

    /// Issues #575 and #600 — the partial-orchestration dispatch, at the altitude
    /// the user meets it: one role's command is wrong, the dispatch reports
    /// failure, and the roles that DID start are left running as orphans in a
    /// directory the rollback then deletes underneath them.
    ///
    /// Roles 0 and 1 run `cat` (alive on stdin, no LLM tokens); role 2 is an
    /// unresolvable absolute path, which is the cheapest way to make exactly one
    /// later role fail. Three roles rather than two so the failure is genuinely
    /// "a later role", with more than one survivor behind it.
    ///
    /// Pre-fix RED on the orphan assertions: `spawn` `?`s out of its role loop, so
    /// the two `cat` children stay live in the registry with no `SpawnHandle` for
    /// the caller to close them with (#600), while `handle_dispatch`'s rollback
    /// force-removes the worktree those children are rooted in (#575).
    // `cat` as the stand-in role, and POSIX spawn/termination semantics: the
    // fast tier runs on Windows CI too, where a bare `cat` fails to exec and
    // would turn "a LATER role failed" into "the first role failed" — the test
    // would still pass, for none of the reasons it exists.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_partial_orchestration_dispatch_leaves_no_orphans_and_no_deleted_cwd() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        std::fs::write(
            repo.join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"partial-orch\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker-one\"\ncommand = \"cat\"\n\n\
             [[orchestrations.roles]]\nname = \"worker-two\"\n\
             command = \"/nonexistent/dot-agent-deck-575\"\n",
        )
        .unwrap();
        // The shape is resolved from the CALLER's repo, but the roles run in a HEAD
        // checkout, so the config has to be committed.
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git available");
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", "add orchestration"]);

        let (event_tx, _rx) = tokio::sync::broadcast::channel(64);
        let state: crate::state::SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let ctx = DispatchContext {
            working_dir: repo.clone(),
            registry: Arc::new(AgentPtyRegistry::new()),
            event_tx,
            worktrees: new_worktree_registry(),
            default_command: None,
            state: Some(state.clone()),
        };

        let result = handle_dispatch(
            &ctx,
            "partial-unit",
            "Verify the partial-spawn rollback.",
            Some(&crate::event::DispatchShape::Orchestration { name: None }),
        )
        .await;

        // Reclaim the sibling worktree regardless of the assertions below.
        struct Guard(std::path::PathBuf, Arc<AgentPtyRegistry>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.1.shutdown_all();
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(result.worktree_dir.clone(), ctx.registry.clone());

        assert!(
            !result.success,
            "precondition: role 2 cannot be exec'd, so the dispatch must fail: {}",
            result.message
        );

        // #600: nothing the failed dispatch started may outlive it. The caller got
        // no `SpawnHandle`, so anything still live here is unreachable by any
        // close path the user has.
        let live = ctx.registry.agent_records();
        assert!(
            live.is_empty(),
            "a failed dispatch must leave no live agents behind; found {} orphan(s): {:?}",
            live.len(),
            live.iter()
                .map(|r| (r.display_name.clone(), r.cwd.clone()))
                .collect::<Vec<_>>()
        );

        // …and no routing state for panes that no longer exist.
        let guard = state.read().await;
        assert!(
            guard.pane_role_map.is_empty(),
            "the rolled-back roles must leave no role-map entries: {:?}",
            guard.pane_role_map
        );
        assert!(
            guard.orchestrator_pane_ids.is_empty(),
            "…nor an orchestrator marker: {:?}",
            guard.orchestrator_pane_ids
        );
        drop(guard);

        // #575: the rollback may only reclaim the tree once nothing is rooted in
        // it — which, after the teardown above, is the case, so the slot is freed
        // exactly as it was for a spawn that never started an agent at all.
        assert!(
            !crate::issue_dispatch_run::worktree_still_in_use(
                &ctx.registry.agent_records(),
                &result.worktree_dir
            ),
            "no agent may still be rooted in the dispatched worktree"
        );
        assert!(
            !result.worktree_dir.exists(),
            "with nothing live in it, the worktree must still be reclaimed"
        );
        assert!(
            !branch_exists(&repo, "agent/dispatch-partial-unit"),
            "…and its branch deleted, so the name is not wedged"
        );
    }

    /// Control for the test above (issue #575): the same rollback, on the case its
    /// original comment described CORRECTLY — the FIRST role fails, so no agent
    /// ever started and nothing is rooted in the tree. The worktree and its branch
    /// must still be reclaimed, exactly as before the guard existed.
    ///
    /// Without this, "the tree survived" would be indistinguishable from "the
    /// guard fires on every rollback", which would wedge every dispatch name after
    /// a typo'd command.
    // Unix for the same reason as its sibling above: on Windows the `cat` role
    // would fail too, so "nothing started" would hold by accident.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_rollback_with_nothing_started_still_reclaims_the_worktree_and_branch() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        std::fs::write(
            repo.join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"doomed-orch\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\n\
             command = \"/nonexistent/dot-agent-deck-575-first\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"cat\"\n",
        )
        .unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git available");
        };
        run(&["add", "-A"]);
        run(&["commit", "-qm", "add orchestration"]);

        let (event_tx, _rx) = tokio::sync::broadcast::channel(64);
        let ctx = DispatchContext {
            working_dir: repo.clone(),
            registry: Arc::new(AgentPtyRegistry::new()),
            event_tx,
            worktrees: new_worktree_registry(),
            default_command: None,
            state: None,
        };

        let result = handle_dispatch(
            &ctx,
            "doomed-unit",
            "This one never starts an agent at all.",
            Some(&crate::event::DispatchShape::Orchestration { name: None }),
        )
        .await;

        assert!(!result.success, "precondition: role 0 cannot be exec'd");
        assert!(
            ctx.registry.agent_records().is_empty(),
            "precondition: no role started, so nothing can be rooted in the tree"
        );
        assert!(
            !result.worktree_dir.exists(),
            "a rollback with nothing rooted in the tree must still remove it: {}",
            result.message
        );
        assert!(
            !branch_exists(&repo, "agent/dispatch-doomed-unit"),
            "…and delete its branch, so the name is reusable"
        );
        assert!(
            !result.message.contains("LEFT IN PLACE"),
            "the retention note must not fire when nothing is rooted in the tree: {}",
            result.message
        );
    }

    /// Issue #575 proper: the rollback guard, exercised with a LIVE agent actually
    /// rooted in the tree.
    ///
    /// Issue #600's teardown makes that state unreachable through `handle_dispatch`
    /// (the test above proves the partial spawn now tears itself down), so this
    /// pins the guard at its own seam — a survivor must cost a retained directory,
    /// never a deleted working directory. `cat` stands in for an agent: it holds
    /// the cwd open on stdin and costs no LLM tokens.
    // `cat` as the stand-in role, and POSIX spawn/termination semantics: the
    // fast tier runs on Windows CI too, where a bare `cat` fails to exec and
    // would turn "a LATER role failed" into "the first role failed" — the test
    // would still pass, for none of the reasons it exists.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_rollback_leaves_a_worktree_that_a_live_agent_is_rooted_in() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let worktree_dir = repo.parent().unwrap().join("repo-dispatch-survivor");
        create_worktree(
            &repo,
            &worktree_dir,
            "agent/dispatch-survivor",
            false,
            Creator::dispatch("survivor"),
        )
        .await
        .unwrap();

        let registry = Arc::new(AgentPtyRegistry::new());
        let worktrees = new_worktree_registry();
        record_worktree(&worktrees, &worktree_dir, &repo, RemovalPolicy::KeepIfDirty);

        // A live PTY child whose cwd IS the dispatched worktree — the role that
        // spawned before a later one failed.
        registry
            .spawn_agent(crate::agent_pty::SpawnOptions {
                command: Some("cat"),
                cwd: Some(&worktree_dir.to_string_lossy()),
                display_name: Some("orchestrator"),
                ..Default::default()
            })
            .expect("spawn the surviving role");
        struct Guard(Arc<AgentPtyRegistry>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.shutdown_all();
            }
        }
        let _guard = Guard(registry.clone());

        let outcome = rollback_dispatched_worktree(
            &registry,
            &worktrees,
            &worktree_dir,
            &repo,
            "agent/dispatch-survivor",
        )
        .await;

        assert_eq!(
            outcome,
            RollbackOutcome::Retained { live: 1 },
            "a tree a live agent is rooted in must be retained, and the count reported"
        );
        assert!(
            worktree_dir.exists(),
            "the rollback must not delete the working directory of a live agent"
        );
        assert!(
            branch_exists(&repo, "agent/dispatch-survivor"),
            "the branch is checked out in the retained tree, so it stays too"
        );
        // The registry entry is what the surviving agent's eventual tab close
        // finds the tree by. Dropping it here would strand the directory forever.
        assert!(
            worktrees.lock().unwrap().contains_key(&worktree_dir),
            "the worktree registry entry must survive so tab-close cleanup can still reclaim it"
        );

        // Control: once that agent is gone, the very same call reclaims the tree.
        registry.shutdown_all();
        let outcome = rollback_dispatched_worktree(
            &registry,
            &worktrees,
            &worktree_dir,
            &repo,
            "agent/dispatch-survivor",
        )
        .await;
        assert_eq!(
            outcome,
            RollbackOutcome::Reclaimed {
                branch_cleanup_failed: false
            },
            "with the agent gone the guard must stop firing"
        );
        assert!(!worktree_dir.exists());
        assert!(!branch_exists(&repo, "agent/dispatch-survivor"));
    }

    /// A shape the repo cannot satisfy must be refused BEFORE any git work, so a
    /// typo leaves no worktree or branch behind and is not reported as a spawn
    /// failure.
    #[tokio::test]
    async fn an_unknown_orchestration_name_is_refused_without_creating_a_worktree() {
        let tmp = crate::test_temp::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);

        let (event_tx, _rx) = tokio::sync::broadcast::channel(64);
        let ctx = DispatchContext {
            working_dir: repo.clone(),
            registry: Arc::new(AgentPtyRegistry::new()),
            event_tx,
            worktrees: new_worktree_registry(),
            default_command: None,
            // These unit tests assert on the worktree + spawn shape, not on
            // delegate routing (`orchestration/dispatch/001` owns that).
            state: None,
        };
        let result = handle_dispatch(
            &ctx,
            "typo-unit",
            "task",
            Some(&crate::event::DispatchShape::Orchestration {
                name: Some("revew".into()),
            }),
        )
        .await;

        assert!(!result.success);
        assert!(
            result.message.contains("revew"),
            "the message must name the requested target: {}",
            result.message
        );
        assert!(
            !result.message.contains("spawn failed"),
            "a validation error must not masquerade as a spawn failure: {}",
            result.message
        );
        assert!(
            !result.worktree_dir.exists(),
            "no worktree may be created for a shape that was refused"
        );
        assert!(
            !branch_exists(&repo, "agent/dispatch-typo-unit"),
            "no branch may be left behind either"
        );
    }

    /// The wire choice maps onto the spawn override, and ABSENT stays absent —
    /// that is what preserves the pre-selector behaviour for an older CLI.
    #[test]
    fn wire_shape_maps_onto_the_spawn_override() {
        use crate::event::DispatchShape;
        assert_eq!(shape_override_of(None), None);
        assert_eq!(
            shape_override_of(Some(&DispatchShape::SingleAgent)),
            Some(SpawnShapeOverride::SingleAgent)
        );
        assert_eq!(
            shape_override_of(Some(&DispatchShape::Orchestration { name: None })),
            Some(SpawnShapeOverride::Orchestration(None))
        );
        assert_eq!(
            shape_override_of(Some(&DispatchShape::Orchestration {
                name: Some("review".into())
            })),
            Some(SpawnShapeOverride::Orchestration(Some("review".into())))
        );
    }

    /// The `shape` field is additive: a payload written by a CLI that predates it
    /// still deserializes, and lands as `None` (= config-derived), so an older
    /// client keeps working against a newer daemon.
    #[test]
    fn dispatch_signal_without_shape_still_deserializes_as_config_derived() {
        let legacy = r#"{"message_type":"dispatch","pane_id":"p1","name":"unit",
                         "task":"do it","timestamp":"2026-08-08T00:00:00Z"}"#;
        let msg: crate::event::DaemonMessage =
            serde_json::from_str(legacy).expect("a pre-selector dispatch payload must still parse");
        match msg {
            crate::event::DaemonMessage::Dispatch(sig) => {
                assert_eq!(sig.name, "unit");
                assert!(
                    sig.shape.is_none(),
                    "an omitted shape must mean config-derived, not a parse failure"
                );
            }
            other => panic!("expected a dispatch message, got {other:?}"),
        }
    }
}
