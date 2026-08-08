//! The spawn primitive (PRD #127 Phase 2A, M2.1).
//!
//! A scheduled fire turns a `working_dir` + `prompt` into a live deck tab. The
//! scheduler lives in the daemon (which owns the PTYs), so this runs in-process
//! against the daemon's [`AgentPtyRegistry`] — it does NOT go over the attach
//! socket. It:
//!
//! 1. **Auto-creates `working_dir`** (`mkdir -p`); on failure it surfaces a
//!    [`NotifyEvent::WorkingDirError`] through the [`Notifier`] seam and returns
//!    an error — the daemon does not crash and sibling tasks keep running.
//! 2. **Branches on the target dir's `.dot-agent-deck.toml`** via the isolated
//!    [`load_config_for_dir`] helper (no reaching into config internals):
//!    `[[orchestrations]]` present → open an orchestration tab and deliver the
//!    prompt to the `orchestrator` role; absent → a single-agent card spawned
//!    with the schedule's `command`. For scheduled fires `command` is always
//!    present — it is required and validated at config load time, so the
//!    `$SHELL` fallback inside [`AgentPtyRegistry::spawn_agent`] (taken when
//!    `command` is `None`) is unreachable from this path. That fallback is
//!    retained in the spawn primitive purely for the new-deck dialog, which
//!    still permits an omitted command.
//! 3. **Reuses the existing spawn path** ([`AgentPtyRegistry::spawn_agent`]) and
//!    delivers the prompt through [`AgentPtyRegistry::write_to_pane_and_submit`]
//!    (payload + CR, routed by `DOT_AGENT_DECK_PANE_ID`), GATED on the spawned
//!    agent's readiness: the fire subscribes to the hook-event broadcast before
//!    spawning and waits for a `SessionStart` matching the pane's `pane_id` +
//!    registry `agent_id` before writing, mirroring the daemon delegate path
//!    ([`crate::state::dispatch_one_owned`]). On a cold first fire this stops
//!    the write from landing before the agent is listening. Commands that emit
//!    no pre-input `SessionStart` (a shell, `cat`, OpenCode) fall through on a
//!    bounded timeout and still receive the prompt.
//!
//! Tab reuse / `new_tab_per_fire` / mid-interaction deliver-on-idle are Phase
//! 2B; [`SpawnRequest`] carries the task `name` so 2B can key a reuse registry
//! on it without reshaping this API. The returned [`SpawnHandle`] is designed
//! with PRD #120 in mind (stable handle + a tab-closed callback seam) so #120
//! needs additions, not breaking changes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership, command_needs_shell_wrap,
};
use crate::event::{AgentEvent, AgentType, BroadcastMsg, DISPLAY_NAME_METADATA_KEY, EventType};
use crate::project_config::{ProjectConfig, load_project_config, resolve_orchestration_name};
use crate::scheduler::{Notifier, NotifyEvent};

/// Fallback buffer delay between spawning the PTY and writing the prompt, used
/// ONLY when [`deliver`] has no hook-event broadcast to gate on (a direct
/// caller without an event bus). The normal scheduler path instead waits for a
/// `SessionStart` readiness signal (see [`deliver`]); this fixed delay just
/// gives the child + the registry's pump reader time to wire up before bytes
/// flow.
const DELIVER_BUFFER_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Prefix every scheduler-spawned pane's `DOT_AGENT_DECK_PANE_ID` carries
/// (PRD #127 N3). Lets the manager dialog's live-status check match
/// schedule-owned panes specifically rather than colliding with a manually
/// spawned agent that happens to share a display name.
pub const SCHEDULE_PANE_ID_PREFIX: &str = "sched-";

/// Monotonic counter making each spawned pane's `DOT_AGENT_DECK_PANE_ID`
/// unique within a daemon lifetime (the prompt-delivery write routes by it).
static PANE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What a fire needs to open a tab. Owned + `Clone` so a scheduler callback can
/// rebuild it on each fire.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Scheduled task name — the Phase 2B reuse-registry key.
    pub task_name: String,
    /// Target directory (already `~`/`$VAR`-expanded by the config loader).
    pub working_dir: String,
    /// Single-agent command; `None` falls back to `$SHELL`. Ignored when the
    /// target dir defines `[[orchestrations]]` (the role commands win).
    pub command: Option<String>,
    /// Prompt delivered into the spawned agent / orchestrator pane.
    pub prompt: String,
    /// PRD #220: a target the CALLER already resolved, used verbatim instead of
    /// deriving one from `working_dir`'s config.
    ///
    /// `dispatch` sets this because it must decide from the CALLER's repo config —
    /// the config the user saw in `--list-targets` — not from the worktree's. The
    /// two differ in two ways that both produced wrong outcomes: the worktree is a
    /// HEAD checkout, so uncommitted config is invisible to it; and
    /// `load_project_config` normalises an unnamed orchestration to its DIRECTORY
    /// BASENAME, so the same entry is `myrepo` from the repo and
    /// `myrepo-dispatch-unit` from the worktree — a name the listing offers and the
    /// spawn can never match.
    ///
    /// Resolving caller-side also means a bad `--orchestration <name>` fails BEFORE
    /// `git worktree add`, instead of burning a create/remove/branch-delete cycle
    /// and surfacing as "failed to spawn agent".
    ///
    /// `None` (the scheduler and issue-dispatch producers) keeps the
    /// config-derived behaviour untouched.
    pub resolved_target: Option<SpawnTarget>,
    /// Compose the ORCHESTRATOR CONTEXT (roles + delegation protocol + this
    /// request's prompt as a task) instead of delivering `prompt` verbatim.
    ///
    /// `true` only for PRD #220 `dispatch`. Without it a dispatched orchestration's
    /// orchestrator is never told that it IS one, so it works alone while every
    /// worker waits for a delegation that cannot arrive.
    ///
    /// Deliberately NOT enabled for the scheduler (#127) or issue-dispatch (#120),
    /// even though both have the identical defect and the composition is now shared.
    /// Turning it on there changes what lands in a SHIPPED feature's pane: the
    /// orchestrator receives a one-line pointer instead of the prompt text, and
    /// three #120/#127 e2e tests assert that text arriving verbatim (a `cat`-based
    /// stub never reads the file). That is #222's job to do deliberately, with those
    /// tests updated as part of it — not a side effect of the dispatcher PR.
    pub compose_orchestrator_context: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("failed to create working_dir {path:?}: {message}")]
    WorkingDir { path: String, message: String },
    #[error("failed to spawn agent: {0}")]
    Agent(String),
}

/// What [`spawn`] opened. `SingleAgent` = one card; `Orchestration` = a tab of
/// role panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnKind {
    SingleAgent,
    Orchestration { name: String },
}

/// One spawned pane: its registry id, its `DOT_AGENT_DECK_PANE_ID`, and (for
/// orchestration panes) the role it fills.
#[derive(Debug, Clone)]
pub struct SpawnedAgent {
    pub id: String,
    pub pane_id: String,
    pub role_name: Option<String>,
}

/// PRD #120 seam: a callback to run when this spawn's tab closes (e.g. per-issue
/// worktree cleanup). Phase 2A stores it; close-detection wiring is deferred.
pub type TabClosedCallback = Box<dyn FnOnce() + Send + 'static>;

/// Stable handle returned by [`spawn`]. Minimal but extensible: PRD #120 should
/// add fields/methods here rather than change the existing shape.
pub struct SpawnHandle {
    /// Scheduled task name (reuse-registry key for Phase 2B).
    pub task_name: String,
    /// What was opened.
    pub kind: SpawnKind,
    /// The spawned panes, in spawn order. For an orchestration the orchestrator
    /// pane is whichever entry has `role_name == Some("orchestrator")` (or the
    /// start role); the prompt was delivered to it.
    pub agents: Vec<SpawnedAgent>,
    /// The `pane_id` (DOT_AGENT_DECK_PANE_ID) the prompt was delivered to — the
    /// single agent pane, or the orchestrator role pane for an orchestration.
    /// PRD #127 M2.2 reuse re-delivers subsequent fires into this pane.
    pub delivery_pane_id: String,
    /// PRD #120 cleanup seam. `None` until a caller registers one via
    /// [`SpawnHandle::on_tab_closed`].
    pub on_tab_closed: Option<TabClosedCallback>,
}

impl SpawnHandle {
    /// Register a tab-closed cleanup callback (PRD #120). Phase 2A only stores
    /// it — the close-detection that fires it lands with #120 / Phase 2B.
    pub fn on_tab_closed(&mut self, cb: TabClosedCallback) {
        self.on_tab_closed = Some(cb);
    }
}

/// Isolated config lookup for a spawn target directory (PRD Risk: the scheduler
/// must not reach into config internals). Returns `None` when the directory has
/// no `.dot-agent-deck.toml` or it fails to parse — both mean "single-agent".
pub fn load_config_for_dir(dir: &Path) -> Option<ProjectConfig> {
    load_project_config(dir).ok().flatten()
}

/// One orchestration role to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSpawn {
    pub role_index: usize,
    pub role_name: String,
    pub command: String,
    pub is_start_role: bool,
}

/// The branch decision: orchestration tab vs single-agent card. Pure data so it
/// is unit-testable independent of the PTY/registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnTarget {
    /// A single-agent card. `command` is the schedule's command; `None` =
    /// `$SHELL` (resolved by the spawn path, mirroring the new-deck dialog).
    SingleAgent { command: Option<String> },
    /// An orchestration tab rooted at the target dir.
    ///
    /// `config` is the CHOSEN orchestration's own config, carried through so the
    /// spawn can compose the orchestrator's context (roles + delegation protocol)
    /// without re-finding it by name — re-resolution is what let the listing and
    /// the spawn disagree in the first place. See
    /// [`crate::orchestrator_context::prepare_orchestrator_prompt`].
    Orchestration {
        name: String,
        roles: Vec<RoleSpawn>,
        config: Box<crate::project_config::OrchestrationConfig>,
    },
}

/// PRD #220: a caller's explicit choice of spawn shape, overriding what the
/// target dir's config would imply.
///
/// Exists because the config-derived default is not knowable by the caller's
/// intent: "work on this feature" wants a team, "verify this PR" wants one
/// agent, and both arrive as the same `dispatch` call into the same repo. Only
/// the user knows which, so `dispatch` asks and passes the answer down. Absent
/// (`None`), [`decide_target`]'s config-derived behaviour is unchanged, which is
/// what keeps the scheduler and issue-dispatch paths exactly as they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnShapeOverride {
    /// Force a single agent even when the dir defines `[[orchestrations]]`.
    SingleAgent,
    /// Force an orchestration: `None` = the dir's first (same as the config
    /// default), `Some(name)` = the one with that `name`, or an error if no
    /// orchestration carries it.
    Orchestration(Option<String>),
}

/// [`decide_target`], with an optional caller override (PRD #220).
///
/// `Err` only for an override naming an orchestration the dir does not define —
/// a silent fallback there would spawn something other than what the user chose,
/// which is exactly the class of surprise this selector exists to remove. The
/// message lists what IS available so the caller can correct itself.
pub fn decide_target_with_override(
    config: Option<&ProjectConfig>,
    dir: &Path,
    schedule_command: Option<&str>,
    over: Option<&SpawnShapeOverride>,
) -> Result<SpawnTarget, String> {
    match over {
        None => Ok(decide_target(config, dir, schedule_command)),
        Some(SpawnShapeOverride::SingleAgent) => Ok(SpawnTarget::SingleAgent {
            command: schedule_command.map(|c| c.to_string()),
        }),
        Some(SpawnShapeOverride::Orchestration(None)) => {
            // The FIRST ROLE-BEARING orchestration, matching what `--list-targets`
            // offers. `decide_target` inspects only `orchestrations.first()`, so a
            // roleless placeholder in slot 0 made the bare form refuse a repo whose
            // second entry was perfectly spawnable — and told the user to add config
            // that already existed.
            let orch = config
                .and_then(|cfg| cfg.orchestrations.iter().find(|o| !o.roles.is_empty()))
                .ok_or_else(|| {
                    format!(
                        "no orchestration with roles is defined in {}: add an \
                         `[[orchestrations]]` section with at least one role, or dispatch a \
                         single agent instead",
                        dir.display()
                    )
                })?;
            Ok(SpawnTarget::Orchestration {
                name: resolve_orchestration_name(&orch.name, dir),
                roles: roles_of(orch),
                config: Box::new(orch.clone()),
            })
        }
        Some(SpawnShapeOverride::Orchestration(Some(want))) => {
            let cfg = config.ok_or_else(|| {
                format!(
                    "no `.dot-agent-deck.toml` in {}, so no orchestration named '{want}' exists",
                    dir.display()
                )
            })?;
            let orch = cfg
                .orchestrations
                .iter()
                // Skip roleless entries: two entries can resolve to the SAME name
                // (e.g. an unnamed `roles = []` plus a real one), and without this
                // filter `find` could return the empty one and refuse a target the
                // listing legitimately offered.
                .filter(|o| !o.roles.is_empty())
                .find(|o| resolve_orchestration_name(&o.name, dir) == *want)
                .ok_or_else(|| {
                    let available: Vec<String> = cfg
                        .orchestrations
                        .iter()
                        .filter(|o| !o.roles.is_empty())
                        .map(|o| resolve_orchestration_name(&o.name, dir))
                        .collect();
                    if available.is_empty() {
                        format!("no orchestration named '{want}', and none are defined")
                    } else {
                        format!(
                            "no orchestration named '{want}'; available: {}",
                            available.join(", ")
                        )
                    }
                })?;
            if orch.roles.is_empty() {
                return Err(format!("orchestration '{want}' defines no roles"));
            }
            Ok(SpawnTarget::Orchestration {
                name: resolve_orchestration_name(&orch.name, dir),
                roles: roles_of(orch),
                config: Box::new(orch.clone()),
            })
        }
    }
}

/// Flatten one orchestration's configured roles into [`RoleSpawn`]s.
fn roles_of(orch: &crate::project_config::OrchestrationConfig) -> Vec<RoleSpawn> {
    orch.roles
        .iter()
        .enumerate()
        .map(|(i, r)| RoleSpawn {
            role_index: i,
            role_name: r.name.clone(),
            command: r.command.clone(),
            is_start_role: r.start,
        })
        .collect()
}

/// Decide what to open from the target dir's config and the schedule's command.
/// `[[orchestrations]]` with at least one role → orchestration; otherwise a
/// single-agent card. `dir` is used only to resolve an unnamed orchestration's
/// name to its cwd-basename (matching the TUI/daemon contract).
pub fn decide_target(
    config: Option<&ProjectConfig>,
    dir: &Path,
    schedule_command: Option<&str>,
) -> SpawnTarget {
    if let Some(cfg) = config
        && let Some(orch) = cfg.orchestrations.first()
        && !orch.roles.is_empty()
    {
        let name = resolve_orchestration_name(&orch.name, dir);
        return SpawnTarget::Orchestration {
            name,
            roles: roles_of(orch),
            config: Box::new(orch.clone()),
        };
    }
    SpawnTarget::SingleAgent {
        command: schedule_command.map(|c| c.to_string()),
    }
}

/// Index (into `roles`) of the role the prompt is delivered to: the one named
/// `orchestrator`, else the start role, else the first. `roles` is assumed
/// non-empty (callers only build an `Orchestration` target with ≥1 role).
pub fn orchestrator_role_index(roles: &[RoleSpawn]) -> usize {
    roles
        .iter()
        .position(|r| r.role_name == "orchestrator")
        .or_else(|| roles.iter().position(|r| r.is_start_role))
        .unwrap_or(0)
}

/// Open a tab for `req` and deliver its prompt. See the module docs for the
/// full contract. On `working_dir` creation or spawn failure, surfaces the
/// reason via `notifier` and returns `Err` without panicking.
///
/// `detach_delivery` controls only WHEN this future returns relative to the
/// prompt-delivery wait — never WHETHER the agent is registered: every pane is
/// always spawned and registered synchronously before `spawn` returns, so a
/// caller that inspects the registry immediately afterwards always sees the
/// agents. When `false` (the #127 single-spawn path), the prompt-delivery wait
/// (which can sit out the multi-second `SessionStart` fallback for a
/// hook-less command) is awaited before returning. When `true` (the PRD #120
/// issue-dispatch path), that wait runs in a detached task so the caller — the
/// scheduler's run-active window — is freed the instant the dispatch WORK is
/// done; a rapid re-fire after a tab close is then not blocked behind the prior
/// run's lingering delivery wait. The prompt is still delivered either way.
pub async fn spawn(
    req: SpawnRequest,
    registry: &Arc<AgentPtyRegistry>,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    detach_delivery: bool,
) -> Result<SpawnHandle, SpawnError> {
    // 1. mkdir -p the working_dir; fail loud via the notifier.
    let dir = Path::new(&req.working_dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        notifier.notify(NotifyEvent::WorkingDirError {
            task: req.task_name.clone(),
            path: req.working_dir.clone(),
            message: e.to_string(),
        });
        return Err(SpawnError::WorkingDir {
            path: req.working_dir.clone(),
            message: e.to_string(),
        });
    }

    // 2. Branch on the target dir's config, unless the caller chose explicitly
    //    (PRD #220 `dispatch --single` / `--orchestration`). A named
    //    orchestration that the dir does not define is an error, never a silent
    //    fallback to something the user did not pick.
    let target = match req.resolved_target.clone() {
        Some(t) => t,
        None => decide_target(
            load_config_for_dir(dir).as_ref(),
            dir,
            req.command.as_deref(),
        ),
    };

    // 3. Spawn + deliver.
    match target {
        SpawnTarget::SingleAgent { command } => {
            let pane_id = next_pane_id(&req.task_name, None);
            // PRD #127 C2: only pin the `-c` wrapper shell to a deterministic
            // `/bin/sh` when the command ACTUALLY needs shell-wrapping (it has
            // whitespace → a shell command line). A single bare word is exec'd
            // directly (no shell), and an omitted command falls back to the
            // daemon's `$SHELL` (mirrors the new-deck dialog) — in neither case
            // do we pin (or leak) a SHELL override.
            let pin_sh = command.as_deref().is_some_and(command_needs_shell_wrap);
            // PRD #127 readiness gate: SUBSCRIBE before spawning so a
            // fast-booting agent's `SessionStart` can't land on the broadcast
            // before our receiver attaches (mirrors
            // `state.rs::dispatch_one_owned`'s subscribe-before-respawn).
            let event_rx = event_tx.map(|tx| tx.subscribe());
            let id = spawn_one(
                registry,
                command.as_deref(),
                &req.working_dir,
                &pane_id,
                None,
                &req.task_name,
                pin_sh,
                notifier,
            )?;
            // PRD #127 finding #2: surface this single-agent card LIVE to any
            // already-attached TUI (the daemon otherwise only hydrates its
            // agents at TUI startup). Reuses the existing hook-event broadcast
            // — no new broadcast variant. The ORCHESTRATION branch below surfaces
            // its tab LIVE too (PRD #120) but needs the structural membership a
            // flat `SessionStart` can't carry, so it rides the typed
            // [`BroadcastMsg::OrchestrationSurface`] variant instead of this
            // synthetic-`SessionStart` path.
            if let Some(tx) = event_tx {
                surface_spawned_pane(
                    tx,
                    &pane_id,
                    &req.working_dir,
                    command.as_deref(),
                    &req.task_name,
                );
            }
            run_delivery(
                registry,
                pane_id.clone(),
                id.clone(),
                event_rx,
                req.prompt.clone(),
                detach_delivery,
            )
            .await;
            Ok(SpawnHandle {
                task_name: req.task_name,
                kind: SpawnKind::SingleAgent,
                agents: vec![SpawnedAgent {
                    id,
                    pane_id: pane_id.clone(),
                    role_name: None,
                }],
                delivery_pane_id: pane_id,
                on_tab_closed: None,
            })
        }
        SpawnTarget::Orchestration {
            name,
            roles,
            config: orch_config,
        } => {
            let orch_idx = orchestrator_role_index(&roles);
            let mut agents = Vec::with_capacity(roles.len());
            // PRD #127 readiness gate: SUBSCRIBE before any pane is spawned so
            // the orchestrator pane's `SessionStart` can't be missed regardless
            // of spawn order (the orchestrator role is not necessarily first).
            let event_rx = event_tx.map(|tx| tx.subscribe());
            // PRD #140 M1.3: one instance token per orchestration spawn
            // request, minted before the role loop and stamped on every
            // role's membership — the daemon-initiated counterpart of the
            // interactive mint in `tab.rs`. Two scheduled fires of the same
            // orchestration in the same working dir are then two distinct
            // routing groups instead of one ambiguous `(name, cwd)` identity.
            let orchestration_id = crate::agent_pty::mint_orchestration_id();
            for role in &roles {
                let pane_id = next_pane_id(&req.task_name, Some(role.role_index));
                let membership = TabMembership::Orchestration {
                    name: name.clone(),
                    role_index: role.role_index,
                    role_name: role.role_name.clone(),
                    is_start_role: role.is_start_role,
                    orchestration_cwd: Some(req.working_dir.clone()),
                    display_title: None,
                    orchestration_id: Some(orchestration_id.clone()),
                };
                let id = spawn_one(
                    registry,
                    Some(&role.command),
                    &req.working_dir,
                    &pane_id,
                    Some(membership),
                    &req.task_name,
                    false,
                    notifier,
                )?;
                agents.push(SpawnedAgent {
                    id,
                    pane_id,
                    role_name: Some(role.role_name.clone()),
                });
            }
            // PRD #120: surface this orchestration LIVE to any already-attached
            // TUI. Unlike the single-agent card above (a synthetic
            // `SessionStart`), a multi-role tab can only be rebuilt from the
            // structural membership the TUI's partition / open-orchestration
            // machinery consumes, so we push it via the typed
            // `BroadcastMsg::OrchestrationSurface` variant. The TUI attaches each
            // role's PTY and builds the tab mid-session. Best-effort: `send`
            // errs only when no TUI is attached (the standalone-daemon case).
            if let Some(tx) = event_tx {
                surface_spawned_orchestration(tx, &name, &req.working_dir, &roles, &agents);
            }
            // PRD #222 parity: compose the ORCHESTRATOR CONTEXT, exactly as the
            // interactive `Ctrl+n` path does, instead of delivering the caller's
            // task on its own.
            //
            // Without this the orchestrator was never told that it IS an
            // orchestrator, which roles exist, or how to `delegate` — so it acted
            // on the task alone and every worker sat idle waiting for a delegation
            // that could not arrive. In a six-role repo that is one working agent
            // and five idle ones, and it looks like it worked.
            //
            // The caller's task is folded INTO the context file rather than
            // concatenated onto the pointer line, because a multi-line prompt does
            // not submit reliably through a PTY and task text is arbitrary. If the
            // file cannot be written we fall back to the bare task rather than
            // delivering nothing — a degraded orchestrator still beats a silent one.
            let prompt = if req.compose_orchestrator_context {
                crate::orchestrator_context::prepare_orchestrator_prompt(
                    &orch_config,
                    &req.working_dir,
                    Some(req.prompt.as_str()),
                )
                .unwrap_or_else(|| req.prompt.clone())
            } else {
                // #120 / #127: unchanged — the prompt is delivered verbatim. See
                // `compose_orchestrator_context` for why this is not flipped here.
                req.prompt.clone()
            };
            // Deliver the prompt to the orchestrator role pane, gated on that
            // pane's readiness (its registry agent_id is the gate's match key).
            let delivery_pane_id = agents[orch_idx].pane_id.clone();
            let delivery_agent_id = agents[orch_idx].id.clone();
            run_delivery(
                registry,
                delivery_pane_id.clone(),
                delivery_agent_id,
                event_rx,
                prompt,
                detach_delivery,
            )
            .await;
            Ok(SpawnHandle {
                task_name: req.task_name,
                kind: SpawnKind::Orchestration { name },
                agents,
                delivery_pane_id,
                on_tab_closed: None,
            })
        }
    }
}

/// Spawn one pane via the existing registry path, tagging it with `pane_id` (so
/// the prompt-delivery write can route to it) and optional orchestration
/// `membership`. Surfaces a spawn failure via the notifier.
#[allow(clippy::too_many_arguments)]
fn spawn_one(
    registry: &AgentPtyRegistry,
    command: Option<&str>,
    cwd: &str,
    pane_id: &str,
    membership: Option<TabMembership>,
    task_name: &str,
    pin_sh: bool,
    notifier: &dyn Notifier,
) -> Result<String, SpawnError> {
    let opts = SpawnOptions {
        command,
        cwd: Some(cwd),
        display_name: Some(task_name),
        rows: 24,
        cols: 80,
        env: pane_env(pane_id, pin_sh),
        tab_membership: membership,
        // PRD #127 finding #4: tag the daemon-side registry entry with the
        // agent type inferred from the command (e.g. `claude` → `ClaudeCode`),
        // matching what `surface_spawned_pane` puts on the live card and what
        // TUI-spawned panes register (see `tab.rs`). Without this the daemon
        // stored `None`, so a scheduled card showed e.g. `claude` while live
        // but reverted to "No agent" after a reconnect rebuilt it from
        // `list_agents`. `from_command` returns `None` for bare commands, the
        // same legacy placeholder behavior.
        agent_type: AgentType::from_command(command),
    };
    registry.spawn_agent(opts).map_err(|e| {
        notifier.notify(NotifyEvent::SpawnFailed {
            task: task_name.to_string(),
            message: e.to_string(),
        });
        SpawnError::Agent(e.to_string())
    })
}

/// Build the spawn env for a scheduled pane: always the `DOT_AGENT_DECK_PANE_ID`
/// tag, plus a `SHELL=/bin/sh` *wrapper-choice override* only when `pin_sh`
/// (the command needs shell-wrapping). `agent_pty::spawn` consumes the SHELL
/// override to pick the `-c` shell and does NOT export it into the child env
/// (PRD #127 C2), so a single-word command carries no SHELL at all.
///
/// PRD #163 M1: the pinned shell comes from
/// [`crate::platform::shell::fixed_command_shell`] — still `/bin/sh` on Unix,
/// but `%COMSPEC%` on Windows, where pinning a POSIX path would hand
/// `agent_pty::spawn` a shell that does not exist.
fn pane_env(pane_id: &str, pin_sh: bool) -> Vec<(String, String)> {
    let mut env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())];
    if pin_sh {
        env.push((
            "SHELL".to_string(),
            crate::platform::shell::fixed_command_shell("/bin/sh"),
        ));
    }
    env
}

/// Floor for the `DOT_AGENT_DECK_SESSION_START_WAIT_MS` override (PRD #225
/// hardening). Non-zero on purpose: the whole point of the readiness gate is
/// that it WAITS, and `=0` (or `=1`) turns it back into the unsynchronized write
/// that lost the prompt in the first place — the value would be a
/// `tokio::time::timeout` that expires before any subscriber can be polled, so
/// the fallback fires unconditionally. 100 ms is small enough that no test pays a
/// meaningful cost and large enough that a genuine `SessionStart` already sitting
/// on the broadcast bus still wins the race.
const SESSION_START_WAIT_MIN: Duration = Duration::from_millis(100);

/// How long [`deliver`] waits for the spawned agent's `SessionStart` before
/// falling through and writing the prompt anyway. Defaults to the daemon-wide
/// [`crate::state::SESSION_START_WAIT_TIMEOUT`] (matching the delegate path —
/// PRD #225 M4 sized it from measured Codex boot); overridable via
/// `DOT_AGENT_DECK_SESSION_START_WAIT_MS` (milliseconds)
/// so the e2e scheduler harness can shrink the no-hook fallback instead of
/// paying the full production wait. Mirrors the [`reuse_debounce`] override
/// idiom.
///
/// The override is CLAMPED to `[SESSION_START_WAIT_MIN, ` the production default
/// `]` — i.e. it can only ever *shorten* the wait, and only down to a floor that
/// still waits. Both ends are real failure modes rather than hypotheticals: `=0`
/// reintroduces the PRD #225 prompt loss (the gate stops waiting, so the prompt
/// is written into whatever is running in the PTY, typically still the
/// launcher), and an absurd value (`=86400000`) hangs delivery for the rest of
/// the day for any agent whose native hooks never fire, with no output and no
/// error to explain it. An out-of-range value is clamped with a `warn!` rather
/// than rejected, so a mistyped harness pin degrades to the nearest sane
/// behavior instead of silently breaking prompt delivery. A non-numeric value
/// falls back to the default (also with a `warn!`).
fn session_start_wait_timeout() -> Duration {
    let default = crate::state::SESSION_START_WAIT_TIMEOUT;
    let Ok(raw) = std::env::var("DOT_AGENT_DECK_SESSION_START_WAIT_MS") else {
        return default;
    };
    let Ok(ms) = raw.trim().parse::<u64>() else {
        tracing::warn!(
            value = %raw,
            default_ms = default.as_millis(),
            "DOT_AGENT_DECK_SESSION_START_WAIT_MS is not a non-negative integer \
             number of milliseconds; using the default readiness wait"
        );
        return default;
    };
    let requested = Duration::from_millis(ms);
    // `Ord::clamp` panics when min > max, so the floor yields to the ceiling
    // instead of trusting that the production default stays above it — a future
    // retune of `SESSION_START_WAIT_TIMEOUT` must not be able to turn this into
    // a daemon panic.
    let floor = SESSION_START_WAIT_MIN.min(default);
    let clamped = requested.clamp(floor, default);
    if clamped != requested {
        tracing::warn!(
            requested_ms = requested.as_millis(),
            clamped_ms = clamped.as_millis(),
            min_ms = floor.as_millis(),
            max_ms = default.as_millis(),
            "DOT_AGENT_DECK_SESSION_START_WAIT_MS is out of range; clamped. \
             The override may only shorten the readiness wait, and never below \
             a floor that still waits"
        );
    }
    clamped
}

/// Deliver the prompt into a freshly-spawned pane, gated on the agent's
/// readiness. Delivery failure is logged, not fatal — the tab is already open.
///
/// PRD #127 readiness-gate fix: on a cold first fire the old flat
/// [`DELIVER_BUFFER_DELAY`] write could land before the agent was listening, so
/// the prompt was dropped on the floor. This mirrors the daemon delegate path
/// ([`crate::state::dispatch_one_owned`]): the caller has already SUBSCRIBED to
/// the hook-event broadcast *before* spawning (so a fast-booting agent's
/// `SessionStart` can't be missed), and here we WAIT for a `SessionStart`
/// matching this pane's `pane_id` AND the registry `agent_id` before writing,
/// up to [`SESSION_START_WAIT_TIMEOUT`]. Commands that emit no pre-input
/// `SessionStart` (bare `cat`, OpenCode) fall through on the timeout and are
/// delivered anyway — exactly the fallback the delegate/respawn path uses.
///
/// `event_rx == None` (a direct caller with no event bus) preserves the legacy
/// short fixed buffer delay so the child + pump reader are wired before bytes
/// flow.
/// Run the prompt-delivery wait either inline (await it) or detached (spawn it
/// onto a background task and return immediately). The agent is already
/// registered by the time this is called; detaching only frees the caller from
/// the (possibly multi-second) `SessionStart` fallback wait. See [`spawn`]'s
/// `detach_delivery` parameter for why the issue-dispatch path detaches.
async fn run_delivery(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: String,
    agent_id: String,
    event_rx: Option<broadcast::Receiver<BroadcastMsg>>,
    prompt: String,
    detach: bool,
) {
    if detach {
        let registry = Arc::clone(registry);
        tokio::spawn(async move {
            deliver(&registry, &pane_id, &agent_id, event_rx, &prompt).await;
        });
    } else {
        deliver(registry, &pane_id, &agent_id, event_rx, &prompt).await;
    }
}

async fn deliver(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    agent_id: &str,
    event_rx: Option<broadcast::Receiver<BroadcastMsg>>,
    prompt: &str,
) {
    match event_rx {
        Some(mut rx) => {
            let timeout = session_start_wait_timeout();
            let observed =
                crate::state::wait_for_session_start(&mut rx, pane_id, agent_id, timeout).await;
            if !observed {
                tracing::debug!(
                    pane_id,
                    timeout_ms = timeout.as_millis(),
                    "scheduled spawn: SessionStart wait timed out; \
                     delivering prompt via fallback path"
                );
            }
        }
        None => tokio::time::sleep(DELIVER_BUFFER_DELAY).await,
    }
    if let Err(e) = registry.write_to_pane_and_submit(pane_id, prompt).await {
        tracing::warn!(pane_id, error = %e, "scheduled prompt delivery failed");
    }
}

/// PRD #127 finding #2: surface a freshly-spawned single-agent scheduled pane
/// to any ALREADY-ATTACHED TUI by publishing a synthetic `SessionStart`
/// through the daemon's EXISTING hook-event broadcast — the same channel a
/// real agent's `SessionStart` hook rides. Reusing that fan-out (rather than
/// adding a new broadcast variant) brings bare commands (a shell, `cat`) that
/// emit no hook of their own to card-surfacing parity with hook-emitting
/// agents: before this, a scheduler fire registered an agent in the daemon
/// that an attached dashboard never painted, because the TUI only hydrates
/// daemon agents at startup.
///
/// `agent_id` is deliberately `None`: a later real `SessionStart` hook from
/// the spawned agent (carrying the daemon registry id) then SUPERSEDES this
/// placeholder via `AppState::apply_event`'s retire-on-agent-id-mismatch path,
/// instead of leaving a duplicate card beside it. `cwd` is the spawn target so
/// the dashboard renders the card with the working-dir basename. Delivery is
/// best-effort: `send` errs only when there are no subscribers (no TUI
/// attached), which is the expected standalone-daemon case.
///
/// PRD #127 finding #2 followup: `task_name` rides on the event's
/// [`DISPLAY_NAME_METADATA_KEY`] so the attached TUI titles the live card with
/// the schedule's friendly name. Without it the card fell back to the
/// truncated pane id (`sched-<name>-<n>`'s 11-char prefix). The daemon already
/// stores this name as the registry `display_name`, so a reconnect titled the
/// card correctly; this brings the live path to parity.
fn surface_spawned_pane(
    event_tx: &broadcast::Sender<BroadcastMsg>,
    pane_id: &str,
    cwd: &str,
    command: Option<&str>,
    task_name: &str,
) {
    let mut metadata = HashMap::new();
    metadata.insert(DISPLAY_NAME_METADATA_KEY.to_string(), task_name.to_string());
    let event = AgentEvent {
        session_id: pane_id.to_string(),
        agent_type: AgentType::from_command(command).unwrap_or(AgentType::None),
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: Some(cwd.to_string()),
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata,
        pane_id: Some(pane_id.to_string()),
        agent_id: None,
        agent_version: None,
        schema_version: None,
        live_target: None,
    };
    let _ = event_tx.send(BroadcastMsg::Event(event));
}

/// PRD #120: surface a freshly-spawned ORCHESTRATION to attached TUIs by
/// publishing its structural membership through the daemon's existing
/// `BroadcastMsg` fan-out as a typed [`BroadcastMsg::OrchestrationSurface`].
///
/// Where [`surface_spawned_pane`] forges a flat `SessionStart` (enough for a
/// single dashboard card), an orchestration TAB groups several role panes and
/// can only be rebuilt from the per-role index / name / start-flag / cwd the
/// TUI's `open_orchestration_tab_with_existing_role_panes` machinery consumes.
/// `roles` and `agents` are parallel (each role was pushed in iteration order),
/// so the i-th role's spawned pane is `agents[i]`. The `pane_id` is reused
/// TUI-side as the local pane id (so hook routing stays correct) AND is how the
/// TUI attaches to the live PTY — it resolves the pane id through `list_agents`
/// rather than via a registry agent id, so no `agent_id` rides on the wire.
/// Delivery is best-effort: `send` errs only when there are no subscribers.
fn surface_spawned_orchestration(
    event_tx: &broadcast::Sender<BroadcastMsg>,
    name: &str,
    cwd: &str,
    roles: &[RoleSpawn],
    agents: &[SpawnedAgent],
) {
    let surface_roles = roles
        .iter()
        .zip(agents.iter())
        .map(|(role, agent)| crate::event::OrchestrationSurfaceRole {
            pane_id: agent.pane_id.clone(),
            role_index: role.role_index,
            role_name: role.role_name.clone(),
            is_start_role: role.is_start_role,
        })
        .collect();
    // PRD #120 S1: disambiguate concurrent dispatched orchestration tabs. The
    // daemon-initiated path carries no user-typed title, so N concurrent issue
    // dispatches would all paint identically-labelled tabs (the shared
    // orchestration `name`, e.g. `issue-work`) — indistinguishable in the tab
    // strip. Append the per-spawn identity: the cwd basename, which for issue
    // dispatch is the per-issue worktree `issue-<n>`. `name` stays the PREFIX so
    // the canonical label reads first and survives the tab strip's
    // trailing-ellipsis truncation. When the basename already equals `name` (an
    // unnamed orchestration whose name resolved to its own cwd basename) there's
    // nothing to add — fall back to `None`, i.e. the canonical `name`.
    let display_title = Path::new(cwd)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .filter(|b| b != name)
        .map(|b| format!("{name} · {b}"));
    let surface = crate::event::OrchestrationSurface {
        name: name.to_string(),
        cwd: cwd.to_string(),
        display_title,
        roles: surface_roles,
    };
    let _ = event_tx.send(BroadcastMsg::OrchestrationSurface(surface));
}

/// A fresh, valid `DOT_AGENT_DECK_PANE_ID` for a spawned pane. Sanitizes the
/// task name to the allowed charset and appends a monotonic counter (+ role
/// index for orchestration panes) so concurrent fires never collide.
fn next_pane_id(task_name: &str, role_index: Option<usize>) -> String {
    let n = PANE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let sanitized: String = task_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    match role_index {
        Some(idx) => format!("{SCHEDULE_PANE_ID_PREFIX}{sanitized}-{n}-r{idx}"),
        None => format!("{SCHEDULE_PANE_ID_PREFIX}{sanitized}-{n}"),
    }
}

// ---------------------------------------------------------------------------
// Tab-reuse lifecycle (PRD #127 Phase 2B, M2.2)
// ---------------------------------------------------------------------------

/// Default deliver-on-idle debounce window (PRD #127 Q6 working assumption:
/// ~5s of no user input before a reuse prompt is delivered into a pane the
/// user might be typing in).
pub const DEFAULT_REUSE_DEBOUNCE_MS: u64 = 5000;

/// The deliver-on-idle debounce window. Overridable via
/// `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` (milliseconds) so tests can shrink it
/// without a real ~5s wait; falls back to [`DEFAULT_REUSE_DEBOUNCE_MS`].
pub fn reuse_debounce() -> Duration {
    std::env::var("DOT_AGENT_DECK_REUSE_DEBOUNCE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(DEFAULT_REUSE_DEBOUNCE_MS))
}

/// One reuse-registry entry: the tab a `new_tab_per_fire = false` task last
/// opened. In-memory only (wiped on daemon restart — the first post-restart
/// fire spawns fresh; documented, not persisted).
#[derive(Debug, Clone)]
pub struct ReuseEntry {
    /// Registry ids of the panes this tab spawned — checked for liveness so a
    /// closed/exited tab becomes stale and the next fire spawns fresh.
    pub agent_ids: Vec<String>,
    /// The pane reuse re-delivers into (single agent, or orchestrator role).
    pub delivery_pane_id: String,
}

/// Daemon-owned, in-memory reuse registry keyed by scheduled task `name`
/// (PRD #127 Q8). Threaded into the scheduler callback factory so each fire
/// can consult/record it. Wiped on daemon restart.
pub type ReuseRegistry = Arc<Mutex<HashMap<String, ReuseEntry>>>;

/// Construct an empty reuse registry.
pub fn new_reuse_registry() -> ReuseRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// A live tab already recorded for a task name, with its current liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingTab {
    pub pane_id: String,
    pub live: bool,
}

/// Reuse-vs-spawn decision (pure, unit-tested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReuseDecision {
    /// Open a brand-new tab (and, for a reuse task, record it).
    SpawnFresh,
    /// Re-deliver into the existing pane.
    Reuse { pane_id: String },
}

/// Decide whether a fire reuses an existing tab or spawns fresh.
/// `new_tab_per_fire == true` always spawns fresh; otherwise reuse iff a
/// recorded tab for the name is still live (a stale/closed one → fresh).
pub fn decide_reuse(new_tab_per_fire: bool, existing: Option<ExistingTab>) -> ReuseDecision {
    if new_tab_per_fire {
        return ReuseDecision::SpawnFresh;
    }
    match existing {
        Some(tab) if tab.live => ReuseDecision::Reuse {
            pane_id: tab.pane_id,
        },
        _ => ReuseDecision::SpawnFresh,
    }
}

/// Deliver-now-vs-queue decision for a reuse fire (pure, unit-tested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryDecision {
    /// The pane is idle — deliver immediately.
    Now,
    /// The user typed recently — wait `after` before re-checking.
    Queue { after: Duration },
}

/// If the last user keystroke is older than `debounce`, deliver now; otherwise
/// queue until the remaining window elapses. `last_input == None` (the user
/// never typed) is always "now".
pub fn decide_delivery(
    last_input: Option<Instant>,
    now: Instant,
    debounce: Duration,
) -> DeliveryDecision {
    match last_input {
        Some(t) => {
            let elapsed = now.saturating_duration_since(t);
            if elapsed >= debounce {
                DeliveryDecision::Now
            } else {
                DeliveryDecision::Queue {
                    after: debounce - elapsed,
                }
            }
        }
        None => DeliveryDecision::Now,
    }
}

/// PRD #127 C1: hard cap on how long a reuse prompt may sit queued behind
/// continuous user typing. Once `started` is this far in the past the prompt is
/// delivered regardless of ongoing input, so it can't be starved forever.
/// Mirrors the 60s hard timeout `process_pending_seed_prompts` uses.
pub const REUSE_DELIVERY_HARD_TIMEOUT: Duration = Duration::from_secs(60);

/// Deliver-on-idle decision WITH the hard-timeout safety net: once the total
/// wait since `started` reaches `hard_cap`, force delivery (`Now`) regardless
/// of recent input; otherwise defer to the debounce ([`decide_delivery`]). Pure
/// so the timeout policy is unit-testable without wall-clock waits.
pub fn decide_delivery_capped(
    last_input: Option<Instant>,
    now: Instant,
    started: Instant,
    debounce: Duration,
    hard_cap: Duration,
) -> DeliveryDecision {
    if now.saturating_duration_since(started) >= hard_cap {
        return DeliveryDecision::Now;
    }
    decide_delivery(last_input, now, debounce)
}

/// Fire a scheduled task: reuse the existing tab when allowed, else spawn a
/// fresh one and record it. This is what the daemon's scheduler callback calls
/// (instead of `spawn` directly) once `new_tab_per_fire` and the reuse registry
/// are in play. The `spawn` primitive's signature is unchanged — reuse is
/// daemon-side state layered on top.
pub async fn spawn_or_reuse(
    req: SpawnRequest,
    new_tab_per_fire: bool,
    registry: &Arc<AgentPtyRegistry>,
    reuse: &ReuseRegistry,
    notifier: &dyn Notifier,
    debounce: Duration,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
) -> Result<(), SpawnError> {
    // Snapshot the reuse decision under the lock (don't hold it across awaits).
    let decision = {
        let map = reuse.lock().unwrap();
        let existing = map.get(&req.task_name).map(|e| ExistingTab {
            pane_id: e.delivery_pane_id.clone(),
            // PRD #127 C3: gate reuse on the liveness of the SPECIFIC pane the
            // prompt is delivered into (orchestrator role / single-agent pane),
            // NOT "any agent for the task" — otherwise we'd re-deliver into a
            // dead orchestrator pane while a sibling role pane is still alive.
            live: registry.pane_is_live(&e.delivery_pane_id),
        });
        decide_reuse(new_tab_per_fire, existing)
    };

    match decision {
        ReuseDecision::Reuse { pane_id } => {
            // Re-deliver into the existing pane, honoring deliver-on-idle.
            deliver_on_idle(registry, &pane_id, &req.prompt, debounce).await;
            Ok(())
        }
        ReuseDecision::SpawnFresh => {
            let task_name = req.task_name.clone();
            // #127 single-spawn keeps awaiting delivery (detach_delivery = false):
            // its callback has no rapid-refire-after-close concern and existing
            // tests expect the prior behavior.
            let handle = spawn(req, registry, notifier, event_tx, false).await?;
            // Record the tab for reuse only when the task opts into reuse.
            if !new_tab_per_fire {
                let entry = ReuseEntry {
                    agent_ids: handle.agents.iter().map(|a| a.id.clone()).collect(),
                    delivery_pane_id: handle.delivery_pane_id.clone(),
                };
                reuse.lock().unwrap().insert(task_name, entry);
            }
            Ok(())
        }
    }
}

/// Deliver `prompt` into `pane_id`, waiting out the deliver-on-idle debounce:
/// if the user keeps typing the window keeps resetting; once the pane is idle
/// (no keystroke within `debounce`) the prompt is written via the ungated
/// `write_to_pane_and_submit`. Skip-if-prior-run-still-active (Phase 1) gives
/// this single-slot semantics per task — a newer fire while one is queued is
/// skipped, and since a static schedule's prompt is identical each fire the
/// delivered prompt is the same regardless.
async fn deliver_on_idle(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    prompt: &str,
    debounce: Duration,
) {
    // PRD #127 C1: bound the total wait so continuous typing can't starve the
    // queued prompt forever; once the hard cap elapses we deliver regardless.
    let started = Instant::now();
    loop {
        let decision = decide_delivery_capped(
            registry.last_user_input_at(pane_id),
            Instant::now(),
            started,
            debounce,
            REUSE_DELIVERY_HARD_TIMEOUT,
        );
        match decision {
            DeliveryDecision::Now => break,
            DeliveryDecision::Queue { after } => {
                // Never sleep past the hard cap — otherwise a long debounce
                // could overshoot the bound on the final wait.
                let remaining_cap = REUSE_DELIVERY_HARD_TIMEOUT.saturating_sub(started.elapsed());
                tokio::time::sleep(after.min(remaining_cap)).await;
            }
        }
    }
    if let Err(e) = registry.write_to_pane_and_submit(pane_id, prompt).await {
        tracing::warn!(pane_id, error = %e, "scheduled reuse prompt delivery failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(toml: &str) -> ProjectConfig {
        toml::from_str(toml).expect("parse project config")
    }

    #[test]
    fn decide_target_single_agent_when_no_config() {
        let dir = Path::new("/tmp/x");
        let t = decide_target(None, dir, Some("claude"));
        assert_eq!(
            t,
            SpawnTarget::SingleAgent {
                command: Some("claude".to_string())
            }
        );
    }

    #[test]
    fn decide_target_single_agent_none_command_means_shell() {
        // `None` command flows through to the spawn path's `$SHELL` fallback.
        let dir = Path::new("/tmp/x");
        let t = decide_target(None, dir, None);
        assert_eq!(t, SpawnTarget::SingleAgent { command: None });
    }

    #[test]
    fn decide_target_single_agent_when_config_has_no_orchestrations() {
        let cfg = parse_config("[[modes]]\nname = \"dev\"\n");
        let dir = Path::new("/tmp/x");
        let t = decide_target(Some(&cfg), dir, Some("cat"));
        assert_eq!(
            t,
            SpawnTarget::SingleAgent {
                command: Some("cat".to_string())
            }
        );
    }

    #[test]
    fn decide_target_orchestration_when_present() {
        let cfg = parse_config(
            "[[orchestrations]]\nname = \"digest\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"sh\"\n",
        );
        let dir = Path::new("/tmp/x");
        // The schedule command is ignored for the orchestration branch.
        let t = decide_target(Some(&cfg), dir, Some("ignored"));
        match t {
            SpawnTarget::Orchestration { name, roles, .. } => {
                assert_eq!(name, "digest");
                assert_eq!(roles.len(), 2);
                assert_eq!(roles[0].role_name, "orchestrator");
                assert_eq!(roles[0].command, "cat");
                assert!(roles[0].is_start_role);
                assert_eq!(roles[1].role_name, "worker");
                assert_eq!(roles[1].role_index, 1);
            }
            other => panic!("expected orchestration, got {other:?}"),
        }
    }

    // --- PRD #220: the caller's explicit shape override ---

    fn two_orchestration_config() -> ProjectConfig {
        parse_config(
            "[[orchestrations]]\nname = \"digest\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"sh\"\n\n\
             [[orchestrations]]\nname = \"review\"\n\n\
             [[orchestrations.roles]]\nname = \"lead\"\ncommand = \"cat\"\nstart = true\n",
        )
    }

    /// `None` must leave the config-derived behaviour byte-for-byte unchanged —
    /// that is what keeps the scheduler and issue-dispatch producers untouched.
    #[test]
    fn shape_override_absent_matches_plain_decide_target() {
        let cfg = two_orchestration_config();
        let dir = Path::new("/tmp/x");
        for cmd in [Some("claude"), None] {
            assert_eq!(
                decide_target_with_override(Some(&cfg), dir, cmd, None),
                Ok(decide_target(Some(&cfg), dir, cmd))
            );
            assert_eq!(
                decide_target_with_override(None, dir, cmd, None),
                Ok(decide_target(None, dir, cmd))
            );
        }
    }

    /// The "verify these PRs" case: one agent, even though the repo defines
    /// orchestrations. Without the override this dir always yields a team.
    #[test]
    fn shape_override_single_agent_wins_over_config_orchestrations() {
        let cfg = two_orchestration_config();
        let dir = Path::new("/tmp/x");
        assert!(matches!(
            decide_target(Some(&cfg), dir, Some("claude")),
            SpawnTarget::Orchestration { .. }
        ));
        assert_eq!(
            decide_target_with_override(
                Some(&cfg),
                dir,
                Some("claude"),
                Some(&SpawnShapeOverride::SingleAgent)
            ),
            Ok(SpawnTarget::SingleAgent {
                command: Some("claude".to_string())
            })
        );
    }

    /// A bare `--orchestration` takes the dir's first (matching the config
    /// default); a named one picks that orchestration even when it is NOT first.
    #[test]
    fn shape_override_orchestration_by_name_selects_beyond_the_first() {
        let cfg = two_orchestration_config();
        let dir = Path::new("/tmp/x");

        let first = decide_target_with_override(
            Some(&cfg),
            dir,
            None,
            Some(&SpawnShapeOverride::Orchestration(None)),
        );
        assert!(matches!(
            first,
            Ok(SpawnTarget::Orchestration { ref name, .. }) if name == "digest"
        ));

        let second = decide_target_with_override(
            Some(&cfg),
            dir,
            None,
            Some(&SpawnShapeOverride::Orchestration(Some("review".into()))),
        );
        match second {
            Ok(SpawnTarget::Orchestration { name, roles, .. }) => {
                assert_eq!(name, "review");
                assert_eq!(
                    roles.len(),
                    1,
                    "the SECOND orchestration's roles, not the first's"
                );
                assert_eq!(roles[0].role_name, "lead");
            }
            other => panic!("expected the named orchestration, got {other:?}"),
        }
    }

    /// A name the dir does not define must ERROR, never silently fall back —
    /// spawning something other than what the user chose is the exact surprise
    /// this selector exists to remove. The message names what IS available.
    #[test]
    fn shape_override_unknown_orchestration_errors_and_lists_available() {
        let cfg = two_orchestration_config();
        let dir = Path::new("/tmp/x");
        let err = decide_target_with_override(
            Some(&cfg),
            dir,
            None,
            Some(&SpawnShapeOverride::Orchestration(Some("nope".into()))),
        )
        .expect_err("an unknown orchestration name must not silently fall back");
        assert!(err.contains("nope"), "error must name the request: {err}");
        assert!(
            err.contains("digest") && err.contains("review"),
            "error must list the available orchestrations: {err}"
        );
    }

    /// Asking for an orchestration where none is defined errors too, rather than
    /// quietly starting a single agent the user did not ask for.
    #[test]
    fn shape_override_orchestration_without_any_defined_errors() {
        let dir = Path::new("/tmp/x");
        let bare = decide_target_with_override(
            None,
            dir,
            Some("claude"),
            Some(&SpawnShapeOverride::Orchestration(None)),
        );
        assert!(bare.is_err(), "no config → no orchestration to start");

        let modes_only = parse_config("[[modes]]\nname = \"dev\"\n");
        assert!(
            decide_target_with_override(
                Some(&modes_only),
                dir,
                None,
                Some(&SpawnShapeOverride::Orchestration(None))
            )
            .is_err(),
            "a config with modes but no orchestrations → error"
        );
    }

    /// A roleless `[[orchestrations]]` is skipped by `decide_target`, so naming it
    /// must error rather than spawn an empty team.
    #[test]
    fn shape_override_roleless_orchestration_errors() {
        // `roles` carries no serde default, so the empty list must be explicit.
        let cfg = parse_config("[[orchestrations]]\nname = \"empty\"\nroles = []\n");
        let dir = Path::new("/tmp/x");
        assert!(
            decide_target_with_override(
                Some(&cfg),
                dir,
                None,
                Some(&SpawnShapeOverride::Orchestration(Some("empty".into())))
            )
            .is_err(),
            "an orchestration with no roles is not a spawnable target"
        );
        // And it is invisible to `--list-targets`, so it is never offered.
        assert!(
            crate::dispatch::available_orchestrations(Some(&cfg), dir).is_empty(),
            "a roleless orchestration must not be listed as a target"
        );
    }

    #[test]
    fn decide_target_unnamed_orchestration_resolves_to_dir_basename() {
        let cfg = parse_config(
            "[[orchestrations]]\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n",
        );
        let dir = Path::new("/home/u/morning-digest");
        match decide_target(Some(&cfg), dir, None) {
            SpawnTarget::Orchestration { name, .. } => assert_eq!(name, "morning-digest"),
            other => panic!("expected orchestration, got {other:?}"),
        }
    }

    #[test]
    fn orchestrator_role_index_prefers_named_orchestrator() {
        let roles = vec![
            RoleSpawn {
                role_index: 0,
                role_name: "worker".into(),
                command: "sh".into(),
                is_start_role: false,
            },
            RoleSpawn {
                role_index: 1,
                role_name: "orchestrator".into(),
                command: "cat".into(),
                is_start_role: false,
            },
        ];
        assert_eq!(orchestrator_role_index(&roles), 1);
    }

    #[test]
    fn orchestrator_role_index_falls_back_to_start_role_then_first() {
        let start_role = vec![
            RoleSpawn {
                role_index: 0,
                role_name: "lead".into(),
                command: "sh".into(),
                is_start_role: false,
            },
            RoleSpawn {
                role_index: 1,
                role_name: "boss".into(),
                command: "cat".into(),
                is_start_role: true,
            },
        ];
        assert_eq!(orchestrator_role_index(&start_role), 1);

        let neither = vec![RoleSpawn {
            role_index: 0,
            role_name: "solo".into(),
            command: "sh".into(),
            is_start_role: false,
        }];
        assert_eq!(orchestrator_role_index(&neither), 0);
    }

    #[test]
    fn next_pane_id_is_valid_and_unique() {
        use crate::agent_pty::is_valid_pane_id_env;
        let a = next_pane_id("morning digest!", None);
        let b = next_pane_id("morning digest!", None);
        let r = next_pane_id("orch", Some(2));
        assert!(is_valid_pane_id_env(&a), "{a} should be a valid pane id");
        assert!(is_valid_pane_id_env(&b));
        assert!(is_valid_pane_id_env(&r));
        assert_ne!(a, b, "pane ids must be unique across calls");
        assert!(r.ends_with("-r2"));
    }

    #[test]
    fn load_config_for_dir_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_config_for_dir(dir.path()).is_none());
    }

    #[test]
    fn load_config_for_dir_reads_orchestration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"d\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n",
        )
        .unwrap();
        let cfg = load_config_for_dir(dir.path()).expect("config present");
        assert_eq!(cfg.orchestrations.len(), 1);
    }

    // --- Phase 2B reuse decision (M2.2) ---

    #[test]
    fn decide_reuse_new_tab_per_fire_always_spawns_fresh() {
        // Even with a live recorded tab, new_tab_per_fire=true opens fresh.
        let existing = Some(ExistingTab {
            pane_id: "p1".into(),
            live: true,
        });
        assert_eq!(decide_reuse(true, existing), ReuseDecision::SpawnFresh);
    }

    #[test]
    fn decide_reuse_reuses_live_tab_by_default() {
        let existing = Some(ExistingTab {
            pane_id: "p1".into(),
            live: true,
        });
        assert_eq!(
            decide_reuse(false, existing),
            ReuseDecision::Reuse {
                pane_id: "p1".into()
            }
        );
    }

    #[test]
    fn decide_reuse_spawns_fresh_when_no_entry_or_stale() {
        assert_eq!(decide_reuse(false, None), ReuseDecision::SpawnFresh);
        let stale = Some(ExistingTab {
            pane_id: "p1".into(),
            live: false,
        });
        assert_eq!(decide_reuse(false, stale), ReuseDecision::SpawnFresh);
    }

    // --- Phase 2B deliver-on-idle decision (M2.2 / Q6) ---

    #[test]
    fn decide_delivery_now_when_never_typed() {
        let now = Instant::now();
        assert_eq!(
            decide_delivery(None, now, Duration::from_millis(2000)),
            DeliveryDecision::Now
        );
    }

    #[test]
    fn decide_delivery_now_when_input_older_than_debounce() {
        let now = Instant::now();
        let debounce = Duration::from_millis(2000);
        let last = now - Duration::from_millis(2500);
        assert_eq!(
            decide_delivery(Some(last), now, debounce),
            DeliveryDecision::Now
        );
    }

    #[test]
    fn decide_delivery_queues_when_recently_typed() {
        let now = Instant::now();
        let debounce = Duration::from_millis(2000);
        let last = now - Duration::from_millis(500);
        match decide_delivery(Some(last), now, debounce) {
            DeliveryDecision::Queue { after } => {
                // ~1500ms remaining (2000 - 500), allow slack for timing.
                assert!(
                    after <= Duration::from_millis(1500) && after >= Duration::from_millis(1400),
                    "unexpected remaining window: {after:?}"
                );
            }
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    // C1 — the hard-timeout safety net forces delivery once the total wait
    // since `started` reaches the cap, regardless of ongoing typing.
    #[test]
    fn decide_delivery_capped_forces_delivery_past_hard_timeout() {
        let now = Instant::now();
        let debounce = Duration::from_millis(2000);
        let hard_cap = Duration::from_secs(60);
        // User typed 100ms ago (well within debounce) → would normally Queue...
        let last = now - Duration::from_millis(100);
        // ...but `started` is past the hard cap → force Now.
        let started = now - (hard_cap + Duration::from_secs(1));
        assert_eq!(
            decide_delivery_capped(Some(last), now, started, debounce, hard_cap),
            DeliveryDecision::Now
        );

        // Within the cap, recent typing still queues.
        let started_recent = now - Duration::from_secs(1);
        assert!(matches!(
            decide_delivery_capped(Some(last), now, started_recent, debounce, hard_cap),
            DeliveryDecision::Queue { .. }
        ));
    }

    // C2 — a single-word command is not shell-wrapped and gets NO SHELL
    // override; a multi-word command is wrapped and carries the override.
    #[test]
    fn single_word_command_not_wrapped_and_no_shell_injected() {
        assert!(!command_needs_shell_wrap("claude"));
        assert!(command_needs_shell_wrap("touch x; sleep 30"));

        // pane_env: single-word (pin_sh=false) → only the pane-id tag.
        let env = pane_env("sched-x-0", false);
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, DOT_AGENT_DECK_PANE_ID);
        assert!(!env.iter().any(|(k, _)| k == "SHELL"));

        // multi-word (pin_sh=true) → pane-id + the SHELL wrapper override. The
        // *value* is platform-specific (`fixed_command_shell`): Unix pins the
        // deterministic POSIX `/bin/sh`, Windows has no such shell to pin and
        // resolves `%COMSPEC%` (else `cmd.exe`) instead. Asserting the real value
        // on each platform rather than skipping the Windows half — the expectation
        // is restated here independently, not read back out of the seam.
        let env = pane_env("sched-x-1", true);
        assert_eq!(env.len(), 2);
        let shell = env
            .iter()
            .find(|(k, _)| k == "SHELL")
            .map(|(_, v)| v.as_str())
            .expect("a wrapped command must carry the SHELL override");
        #[cfg(unix)]
        assert_eq!(shell, "/bin/sh");
        #[cfg(windows)]
        {
            let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
            assert_eq!(
                shell, comspec,
                "on Windows the pinned shell is %COMSPEC% (else cmd.exe), never a POSIX path"
            );
        }
    }

    // finding #2 — the synthetic SessionStart surfaced to attached TUIs is a
    // SessionStart for the spawned pane, rooted at the spawn cwd, with
    // `agent_id == None` so a later real hook supersedes (not duplicates) it,
    // and carrying the schedule's friendly name so the live card titles itself
    // with the name rather than the truncated pane id.
    #[test]
    fn surface_spawned_pane_emits_session_start_for_attached_tuis() {
        let (tx, mut rx) = broadcast::channel(8);
        surface_spawned_pane(
            &tx,
            "sched-morning-digest-0",
            "/tmp/scratch/runbox",
            Some("cat"),
            "morning-digest",
        );
        let BroadcastMsg::Event(e) = rx.try_recv().expect("a broadcast must be queued") else {
            panic!("expected a BroadcastMsg::Event");
        };
        assert_eq!(e.event_type, EventType::SessionStart);
        assert_eq!(e.pane_id.as_deref(), Some("sched-morning-digest-0"));
        assert_eq!(e.cwd.as_deref(), Some("/tmp/scratch/runbox"));
        assert!(
            e.agent_id.is_none(),
            "agent_id must be None so a real SessionStart hook supersedes the placeholder"
        );
        assert_eq!(
            e.metadata
                .get(DISPLAY_NAME_METADATA_KEY)
                .map(String::as_str),
            Some("morning-digest"),
            "the friendly name must ride on the event so the live card titles itself with it"
        );
    }

    #[test]
    fn surface_spawned_pane_send_is_noop_without_subscribers() {
        // The standalone-daemon case (no attached TUI): `send` errs, swallowed.
        let (tx, rx) = broadcast::channel::<BroadcastMsg>(8);
        drop(rx);
        surface_spawned_pane(&tx, "sched-x-0", "/tmp/x", None, "x");
    }

    /// PRD #225 hardening: the readiness-wait override may shorten the wait but
    /// can neither disable it (`=0` reintroduced the prompt loss the PRD fixed)
    /// nor stretch it past the production fallback, and a non-numeric value falls
    /// back to the default rather than panicking. The e2e harness's 5000 ms pin
    /// must survive the clamp untouched.
    #[test]
    fn session_start_wait_override_is_clamped_to_a_sane_range() {
        // Serialize against any other test reading this process-global env var.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DOT_AGENT_DECK_SESSION_START_WAIT_MS").ok();
        let default = crate::state::SESSION_START_WAIT_TIMEOUT;
        for (raw, expected) in [
            // The pin `tests/common` sets for the e2e scheduler harness.
            ("5000", Duration::from_millis(5000)),
            // Gate-disabling values are lifted to the floor.
            ("0", SESSION_START_WAIT_MIN),
            ("1", SESSION_START_WAIT_MIN),
            // A day of hanging delivery is capped at the production fallback.
            ("86400000", default),
            // Unparseable → default, no panic.
            ("soon", default),
            ("-1", default),
            ("", default),
        ] {
            // SAFETY: lock held for the duration; restored below.
            unsafe {
                std::env::set_var("DOT_AGENT_DECK_SESSION_START_WAIT_MS", raw);
            }
            assert_eq!(
                session_start_wait_timeout(),
                expected,
                "DOT_AGENT_DECK_SESSION_START_WAIT_MS={raw:?} must resolve to {expected:?}"
            );
        }
        // SAFETY: same lock; restore.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SESSION_START_WAIT_MS", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SESSION_START_WAIT_MS"),
            }
        }
    }

    #[test]
    fn reuse_debounce_honors_env_override() {
        // Serialize against any other test reading this process-global env var.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DOT_AGENT_DECK_REUSE_DEBOUNCE_MS").ok();
        // SAFETY: lock held for the duration; restored below.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_REUSE_DEBOUNCE_MS", "1234");
        }
        assert_eq!(reuse_debounce(), Duration::from_millis(1234));
        // SAFETY: same lock; restore.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_REUSE_DEBOUNCE_MS", v),
                None => std::env::remove_var("DOT_AGENT_DECK_REUSE_DEBOUNCE_MS"),
            }
        }
    }
}
