use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent_pty::AgentPtyRegistry;
use crate::event::BroadcastMsg;
use crate::issue_dispatch_run::{
    RemovalPolicy, WorktreeCreation, WorktreeRegistry, create_worktree, record_worktree,
    remove_worktree, run_status,
};
use crate::scheduler::StderrNotifier;
use crate::spawn::{SpawnKind, SpawnRequest, SpawnShapeOverride, spawn};

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

    match create_worktree(&clone_dir, &paths.worktree_dir, &paths.branch, false).await {
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

    match spawn(req, &ctx.registry, &notifier, Some(&ctx.event_tx), false).await {
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
            message: match &handle.kind {
                SpawnKind::Orchestration { name: orch } => format!(
                    "dispatch: spawned isolated orchestration '{orch}' for '{name}' in {}",
                    paths.worktree_dir.display()
                ),
                SpawnKind::SingleAgent => format!(
                    "dispatch: spawned isolated agent for '{name}' in {}",
                    paths.worktree_dir.display()
                ),
            },
        },
        Err(e) => {
            // `Force` on the rollback path, unlike the tab-close path: we created
            // this worktree seconds ago and the agent never started, so there is
            // no user work to protect — and it MUST actually go, or the leftover
            // dir and branch wedge this name for every later dispatch.
            remove_worktree(&paths.worktree_dir, &clone_dir, RemovalPolicy::Force).await;
            // Also delete the branch: `git worktree remove` never deletes it,
            // but on this rollback path the agent never ran so there is no
            // committed work to protect — leaving the branch would wedge this
            // name for every later dispatch.
            let branch_cleanup_failed = run_status(
                "git",
                &[
                    "-C",
                    &clone_dir.to_string_lossy(),
                    "branch",
                    "-D",
                    &paths.branch,
                ],
            )
            .await
            .is_err();

            if branch_cleanup_failed {
                tracing::warn!(
                    branch = %paths.branch,
                    "spawn rollback: failed to delete branch — name may be wedged for future dispatches"
                );
            }

            {
                let mut wts = ctx.worktrees.lock().unwrap_or_else(|e| e.into_inner());
                wts.remove(&paths.worktree_dir);
            }

            let cleanup_note = if branch_cleanup_failed {
                " (cleanup failed: branch may still exist — name may be wedged)"
            } else {
                ""
            };

            DispatchResult {
                worktree_dir: paths.worktree_dir,
                success: false,
                message: format!("dispatch: spawn failed: {e}{cleanup_note}"),
            }
        }
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
            create_worktree(&repo, &paths.worktree_dir, &paths.branch, false).await,
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
            create_worktree(&repo, &paths.worktree_dir, &paths.branch, false).await,
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

        create_worktree(&repo, &paths.worktree_dir, &paths.branch, false)
            .await
            .unwrap();
        remove_worktree(&paths.worktree_dir, &repo, RemovalPolicy::KeepIfDirty).await;
        std::process::Command::new("git")
            .args(["branch", "-D", &paths.branch])
            .current_dir(&repo)
            .output()
            .expect("git available");

        assert_eq!(
            create_worktree(&repo, &paths.worktree_dir, &paths.branch, false).await,
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
        create_worktree(&repo, &paths.worktree_dir, &paths.branch, false)
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
        create_worktree(&repo, &worktree_dir, "agent/issue-7", true)
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
