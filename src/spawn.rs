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

use chrono::{DateTime, Utc};

use crate::agent_pty::{
    AgentPtyError, AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, DeliveryNotice, GuardedSend,
    GuardedSendDetail, SpawnOptions, TabMembership, command_needs_shell_wrap,
};
use crate::event::{AgentEvent, AgentType, BroadcastMsg, DISPLAY_NAME_METADATA_KEY, EventType};
use crate::project_config::{ProjectConfig, load_project_config, resolve_orchestration_name};
use crate::prompt_delivery::{
    AUTOMATIC_PROMPT_DEADLINE, log_prompt_abandoned, log_prompt_confirmed, log_prompt_stopped,
    log_prompt_unconfirmable, log_prompt_unconfirmed, log_prompt_written, mint_delivery_id,
    unconfirmed_retry_delay,
};
use crate::scheduler::{Notifier, NotifyEvent};

/// The `path` field every delivery log line from this module carries, so a
/// daemon log with all three delivery paths writing into it can be read per
/// path. See [`crate::prompt_delivery`].
const DELIVERY_LOG_PATH: &str = "spawn";

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
///
/// `state` is the daemon's [`AppState`](crate::state::AppState). It is what makes
/// a daemon-spawned ORCHESTRATION able to delegate: the role → pane maps
/// `handle_delegate` routes on are populated from here, exactly as the
/// `AttachRequest::StartAgent` handler populates them for a `Ctrl+N` one. `None`
/// (tests, and any caller with no daemon state) spawns as before and simply
/// registers nothing.
///
/// It is NOT what makes a spawned pane's live status visible — issue #454 round
/// 1 briefly made it so, by registering every spawned pane in
/// `managed_pane_ids`, and that is the model this doc used to describe. It was
/// replaced (round 2, reviewer nit F is this paragraph): registration is
/// permanent and pane-scoped, so it survived the child's death, admitted
/// forged reports for a pane with no process behind it, and grew by one entry
/// per short-lived pane. Admission now asks
/// [`AgentPtyRegistry`](crate::agent_pty::AgentPtyRegistry) which GENERATION
/// owns the pane, from before the child is forked until its record is reaped
/// (`crate::state::AgentOwnership`), so a single-agent pane is registered
/// NOWHERE here and needs to be. Only ORCHESTRATION roles still register — that
/// registration exists for role → pane routing, not for admission, and
/// `unregister_pane` on close is what takes it back.
pub async fn spawn(
    req: SpawnRequest,
    registry: &Arc<AgentPtyRegistry>,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    detach_delivery: bool,
    state: Option<&crate::state::SharedState>,
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
                // A single-agent spawn has no role, so its card keeps the task
                // name it always had.
                None,
                pin_sh,
                notifier,
            )?;
            // Issue #454: a single-agent spawn registers NOTHING in the
            // daemon's `AppState`, and does not need to. Its pane's lifecycle
            // reports are admitted because the daemon's admission check asks
            // `AgentPtyRegistry` who it owns (`crate::state::AgentOwnership`),
            // and `spawn_one` has just put this pane there — from before the
            // child existed, via the spawn reservation, until the pane changes
            // hands or the generation's record is reaped. (Issue #454 round 3:
            // a generation whose child has died keeps answering for its OWN
            // pane until one of those two happens, so a `SessionEnd` written in
            // the same breath as the exit is still admitted; see
            // `crate::state::AgentOwnership`.) Before that, `apply_event`
            // dropped every non-`SessionStart` report from a scheduled /
            // dispatched pane and its `daemon status` row showed
            // `STATUS=- TOOL=-` exactly like the dashboard-pane bug.
            //
            // The `surface_spawned_pane` broadcast below is unrelated: it
            // publishes a synthetic `SessionStart` straight onto `event_tx` for
            // attached TUIs and never passes through `daemon::ingest_event`, so
            // the daemon's own `AppState` never sees it.
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
            // The same `Instance` identity `StartAgent` derives from the
            // membership these panes are spawned with, so a dispatched
            // orchestration and a `Ctrl+N` one are scoped by exactly the same
            // rule and `delegate_targets`' identity equality behaves identically
            // for both (PRD #140 M2.0). Minted once, before the loop, because
            // every role of one orchestration shares it.
            let identity = crate::state::OrchestrationIdentity::Instance {
                id: orchestration_id.clone(),
                name: name.clone(),
            };
            for (idx, role) in roles.iter().enumerate() {
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
                    // The card label is the ROLE NAME, not the task name. Every
                    // role of one dispatch shares `task_name`, so labelling them
                    // with it makes N identical cards and the user cannot tell
                    // which is the orchestrator. Matches what the interactive
                    // `Ctrl+n` path puts on each role pane (`tab.rs`).
                    Some(role.role_name.as_str()),
                    false,
                    notifier,
                )?;
                // Tell the DAEMON's AppState who this pane is, so the
                // orchestrator we are about to hand a delegation protocol to can
                // actually use it.
                //
                // Without this a dispatched / scheduled orchestration came up
                // complete and inert: `handle_delegate` looks the sender up in
                // `pane_role_map`, which only the `StartAgent` handler was
                // filling, so every `dot-agent-deck delegate` from one of these
                // orchestrators was dropped with `delegate from unknown pane`
                // and no worker ever received a task
                // (`orchestration/dispatch/001`).
                //
                // Done HERE — synchronously, inside the loop, as each role's
                // spawn lands — rather than after the loop or off the
                // `OrchestrationSurface` broadcast. Off the broadcast there is a
                // window in which a fast orchestrator's first delegate beats its
                // own registration. After the loop there is a second problem
                // (PR review, issue #454 item 4): the loop `?`s out on the first
                // role that fails to spawn, so a failure at role N left roles
                // 0..N already launched and registered NOWHERE, with the daemon
                // holding live children it had no routing entry for. Registering
                // per role keeps state consistent with whatever subset actually
                // started.
                //
                // (The already-spawned roles are still not torn down on that
                // early return, and the caller gets no `SpawnHandle` for them so
                // it cannot close them either. That is `spawn`'s pre-existing
                // error semantics, tracked separately — not something this
                // registration ordering can or should decide.)
                //
                // Issue #454 review, item 5: gated on the SAME validation the
                // `AttachRequest::StartAgent` seam applies. There it is implicit
                // — an id `is_valid_pane_id_env` rejects leaves `pane_id_env`
                // as `None` and the role registration below it never runs — and
                // this seam had no equivalent, so it could register an id the
                // registry itself had refused to retain. `next_pane_id` now
                // honours the cap that makes that unreachable; the check stays
                // as the seam-level agreement rather than as a second place
                // where the rule is stated differently. Registering an
                // unretainable id would be strictly worse than skipping: the
                // registry stores `pane_id_env = None` for it, so
                // `write_to_pane_and_submit` could never route to the pane the
                // role map claimed to have.
                if let Some(state) = state.filter(|_| {
                    crate::agent_pty::is_valid_pane_id_env(&pane_id) || {
                        tracing::warn!(
                            pane_id_len = pane_id.len(),
                            role = %role.role_name,
                            "spawn: refusing to register an orchestration role under a \
                             pane id the registry cannot retain — delegation to this \
                             role will not route"
                        );
                        false
                    }
                }) {
                    state.write().await.register_orchestration_role(
                        &pane_id,
                        &role.role_name,
                        // `orch_idx`, NOT `role.is_start_role`. `orch_idx` is
                        // already this path's authority on which role is the
                        // orchestrator — it is the pane that receives the
                        // orchestrator context and the caller's task below — and
                        // it falls back (role named `orchestrator` → any
                        // `start = true` → role 0) where `is_start_role` alone
                        // would be false for EVERY role of an orchestration whose
                        // toml sets no `start`. Registering on the raw flag would
                        // leave such an orchestration with a context-bearing
                        // orchestrator that is still not in
                        // `orchestrator_pane_ids`, i.e. this same bug for a
                        // narrower input.
                        //
                        // KNOWN, and deliberately not fixed here (PR #466
                        // review, issue #523): the registrar is shared, but this
                        // RULE is not. The `AttachRequest::StartAgent` path still
                        // registers on the raw flag — `tab.rs` sends
                        // `is_start_role: role.start` in the membership — so for
                        // a toml whose role is named `orchestrator` but sets no
                        // `start = true`, a `Ctrl+N` tab still registers no
                        // orchestrator at all and its delegate is rejected. That
                        // path is not what this change set out to fix, and
                        // unifying the rule is not local: `tab.rs` computes a
                        // THIRD answer of its own (`start_role_index`, the bare
                        // `position(|r| r.start).unwrap_or(0)`, with no
                        // name-based fallback) and drives default focus and
                        // orchestrator-prompt delivery off it, so aligning the
                        // three is a user-visible TUI change owing its own tests
                        // — see the issue.
                        idx == orch_idx,
                        identity.clone(),
                        Some(req.working_dir.as_str()),
                    );
                }
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
                // …and give every role card its ROLE NAME, the same way the
                // single-agent branch names its card: a synthetic `SessionStart`
                // carrying the friendly name as metadata.
                //
                // The typed `OrchestrationSurface` above carries `role_name`, but
                // only as tab STRUCTURE — the card title reads the session's
                // `display_name`, which nothing was setting on this path. So a
                // dispatched orchestration rendered `ClaudeCode · 6134822e-f2`
                // (claude's session UUID) on every card while the daemon knew all
                // three role names, and the user could not tell the orchestrator
                // from a worker (`orchestration/dispatch/002`).
                //
                // Sent AFTER the surface so the tab exists before the cards it
                // names, and it cannot disturb the readiness gate below: these
                // events carry `agent_id: None`, while `wait_for_session_start`
                // matches only `Some(<registry id>)`. When the role's real
                // `SessionStart` arrives it supersedes this placeholder and
                // INHERITS the name (PRD #127 finding #2), so the label survives
                // the handover instead of reverting to a UUID.
                for (role, agent) in roles.iter().zip(agents.iter()) {
                    surface_spawned_pane(
                        tx,
                        &agent.pane_id,
                        &req.working_dir,
                        Some(&role.command),
                        &role.role_name,
                    );
                }
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
    registry: &Arc<AgentPtyRegistry>,
    command: Option<&str>,
    cwd: &str,
    pane_id: &str,
    membership: Option<TabMembership>,
    task_name: &str,
    // The friendly label for this pane's CARD, when it differs from the task
    // name — the role name for an orchestration role. `None` keeps the task
    // name. `task_name` stays the notifier's subject either way: a spawn
    // failure is reported against the dispatch, not against one role.
    display_name: Option<&str>,
    pin_sh: bool,
    notifier: &dyn Notifier,
) -> Result<String, SpawnError> {
    let opts = SpawnOptions {
        command,
        cwd: Some(cwd),
        display_name: Some(display_name.unwrap_or(task_name)),
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
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    agent_id: &str,
    event_rx: Option<broadcast::Receiver<BroadcastMsg>>,
    prompt: &str,
) {
    // Issue #424, reviewer finding B9: ONE absolute deadline for the whole
    // delivery, captured BEFORE the readiness wait. `started` used to be minted
    // inside `confirm_prompt_delivery` — i.e. after a readiness wait that is 30 s
    // in production — so an automatic prompt could stay active for ~90 s while
    // the two TUI paths enforce the shared 60 s `AUTOMATIC_PROMPT_DEADLINE` from
    // enqueue. Every wait and every write below is bounded by this one instant.
    let deadline = Instant::now() + AUTOMATIC_PROMPT_DEADLINE;
    // Issue #424, reviewer finding B4: `readiness` is carried past the write. It
    // is not "may we deliver" (the fallback still delivers, exactly as before)
    // but "can this producer report a submitted prompt at all" — which decides
    // whether an unconfirmed write may be RE-submitted. See
    // [`confirm_prompt_delivery`].
    let (mut event_rx, observed) = match event_rx {
        Some(mut rx) => {
            let timeout = session_start_wait_timeout().min(remaining_before(deadline));
            let observed =
                crate::state::wait_for_session_start(&mut rx, pane_id, agent_id, timeout).await;
            if !observed.ready {
                tracing::debug!(
                    pane_id,
                    timeout_ms = timeout.as_millis(),
                    "scheduled spawn: SessionStart wait timed out; \
                     delivering prompt via fallback path"
                );
            }
            (Some(rx), observed)
        }
        None => {
            tokio::time::sleep(DELIVER_BUFFER_DELAY.min(remaining_before(deadline))).await;
            (None, crate::state::SessionStartWait::default())
        }
    };
    let delivery_id = mint_delivery_id(pane_id);
    // Issue #424, reviewer finding B1: the pre-write drain IS the watermark.
    // Everything already queued on the broadcast was already visible before our
    // bytes exist, so a submission the agent made on its own beforehand cannot
    // be mistaken for evidence about this delivery. (Ordering, not causality —
    // an event PRODUCED before the write that arrives after it is the residual
    // tracked in #526.) The generation
    // latch it fills means the very first retry already knows which hook session
    // it is bound to. Also the last chance to notice the pane rebound while we
    // waited out a 30 s readiness timeout.
    //
    // Auditor HIGH / C1: the latch STARTS from the readiness event's own
    // generation. Initializing it to `None` here threw that away, so the first
    // `SessionStart` the confirmation loop happened to see became the binding —
    // and after an unobserved rollover that is the SUCCESSOR conversation. A
    // launcher/wrapper-fork readiness event contributes no generation by
    // design; see `crate::state::SessionStartWait::generation`.
    let mut generation: Option<(String, DateTime<Utc>)> = observed.generation.clone();
    let mut drained_capability = false;
    if let Some(rx) = event_rx.as_mut()
        && let Some(reason) = drain_pre_write_events(
            rx,
            pane_id,
            agent_id,
            &mut generation,
            &mut drained_capability,
        )
    {
        log_prompt_stopped(DELIVERY_LOG_PATH, pane_id, &delivery_id, reason);
        return;
    }
    // Reviewer finding B1: the FIRST write is identity-guarded too. The plain
    // `write_to_pane_and_submit` resolves whichever agent currently owns the
    // pane string, and a pane id is just a string an exited agent frees for the
    // next spawn — so after a multi-second readiness wait it could type this
    // dispatch prompt into a replacement.
    match guarded_submit(registry, pane_id, agent_id, prompt, deadline).await {
        GuardedOutcome::Written => {}
        // Issue #424 H3/H5: the FIRST write of this delivery was refused because
        // the pane's input box already holds bytes of ours that the user has
        // typed since. No confirmation task exists yet, so this used to be a
        // `warn!` into a subscriber that `init_logging_from_env` installs only
        // when `DOT_AGENT_DECK_LOG` is set — a prompt lost with nothing on the
        // card to say so, which is the shape of the issue itself. Report it.
        GuardedOutcome::RefusedUserInput => {
            report_user_input_stop(
                registry,
                pane_id,
                agent_id,
                &delivery_id,
                generation.as_ref(),
            );
            return;
        }
        GuardedOutcome::Refused(reason) => {
            tracing::warn!(
                pane_id,
                delivery_id,
                reason,
                "scheduled prompt delivery refused"
            );
            return;
        }
        GuardedOutcome::Failed(e) => {
            tracing::warn!(pane_id, error = %e, "scheduled prompt delivery failed");
            return;
        }
    }
    log_prompt_written(DELIVERY_LOG_PATH, pane_id, &delivery_id, 1);
    // Issue #424: read from `observed_producer`, not from readiness. A launcher
    // that declares its boot provenance is skipped by the readiness gate but has
    // still named the producer, and that is the only question this answers —
    // whether an unconfirmed write could EVER be confirmed. Keying it off
    // readiness meant an honest bootstrap disarmed its own recovery
    // (`scheduler/dispatch/015`).
    //
    // Issue #424 F4 (auditor HIGH): both inputs to this answer are taken BEFORE
    // the write, and that is the point — `observed_producer` is a producer that
    // was already running when we wrote, and `drained_capability` comes from
    // frames queued before we wrote. The confirmation loop no longer ADDS to it
    // from a later event's declared type: provenance and `AgentType` are
    // producer assertions wherever they appear, so an unmarked start arriving
    // afterwards would otherwise arm the full replacement payload, and the blind
    // submit CRs after it, against a pane that may be a shell.
    let can_report_prompts = observed
        .observed_producer
        .as_ref()
        .is_some_and(crate::prompt_delivery::agent_reports_submitted_prompt)
        || drained_capability;
    // Issue #424 F4: the launcher handoff is STANDING, not capability, so it is
    // recorded rather than folded into the answer above. Arming here instead
    // would put the one replacement payload on the retry schedule's clock —
    // ~500 ms after the write — which for `scheduler/dispatch/015` means typing
    // it into a launcher that has not exec'd the real agent yet, and every
    // attempt after that is a submit-only probe with nothing to submit. What the
    // handoff licenses is accepting the successor WHEN IT ANNOUNCES ITSELF, so
    // the payload goes in exactly when the agent is there to receive it. See
    // [`crate::state::SessionStartWait::launcher_handoff`].
    if observed.launcher_handoff {
        registry.note_launcher_handoff(agent_id);
    }
    match event_rx {
        Some(rx) => {
            // Detached on purpose: the caller (a `dispatch` CLI round trip, a
            // scheduler fire) is freed the instant the bytes are written, exactly
            // as before this change. Only the CONFIRMATION — which legitimately
            // runs for tens of seconds against a Claude Code pane starting five
            // MCP servers — moves to the background.
            let registry = Arc::clone(registry);
            let pane_id = pane_id.to_string();
            let agent_id = agent_id.to_string();
            let prompt = prompt.to_string();
            let task = ConfirmationTask {
                pane_id,
                agent_id,
                prompt,
                delivery_id,
                generation,
                can_report_prompts,
                deadline,
            };
            spawn_confirmation_task(registry, rx, task);
        }
        // No event bus at all (a direct caller without one). Nothing can ever
        // report back, so the write is final.
        None => {
            log_prompt_unconfirmable(
                DELIVERY_LOG_PATH,
                pane_id,
                &delivery_id,
                "no hook-event bus for this delivery",
            );
            // Issue #424 H3: final means final — no retry will consult this
            // delivery's payload record, so release it here rather than leaving
            // it to refuse a later delivery of the same text until the TTL.
            registry.note_payload_settled(pane_id, prompt);
        }
    }
}

/// How long is left before `deadline`, saturating at zero.
fn remaining_before(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// The outcome of one identity-guarded submission attempt, flattening
/// [`GuardedSend`] into the answers this path acts on.
enum GuardedOutcome {
    /// Bytes reached the exact expected agent.
    Written,
    /// The target refused the write and NOTHING was written — or the write was
    /// partial and must not be repeated. Terminal either way.
    Refused(&'static str),
    /// Issue #424 H5 (reviewer MEDIUM): refused by the WRITER-HELD backstop
    /// because the user typed after this loop's own pre-check.
    ///
    /// Terminal like any other refusal, but not interchangeable with one: it is
    /// the only stop this path promises to REPORT on the pane's card, and the
    /// promise used to be broken exactly in the race the backstop exists for.
    /// The caller checks the user-input clock before every attempt precisely so
    /// it can report; a keystroke landing between that check and the guarded
    /// acquisition came back as a bare `Stale`, was logged as "target went
    /// stale", and published nothing. See [`GuardedSendDetail`].
    RefusedUserInput,
    /// Transport error before/around the write.
    Failed(AgentPtyError),
}

/// Issue #424, reviewer finding B1 / auditor HIGH #1: submit `prompt` into
/// `pane_id` bound to the EXACT `agent_id` and hook `generation` it was written
/// for, re-validated after the writer lock is taken.
///
/// Two guards, and both matter for a different race:
///
/// * `expected_agent_id` catches a pane that exited and was respawned/rebound
///   between attempts — [`AgentPtyRegistry`] explicitly supports same-pane
///   respawn, and the pane id is reusable, so the unguarded write would have
///   found the successor's writer and typed a stranger's task into it.
/// * `revalidate` catches a pane whose close has BEGUN while we waited for the
///   writer — the same recheck the delegate idle-watch performs at the same
///   point, and the only liveness fact this path can consult without the daemon's
///   `AppState`.
///
/// A SAME-agent `/clear` or thread restart is invisible to both: it rolls the
/// hook session over while the registry identity stays put. Only the event
/// stream sees that, so it is caught by the latched generation in
/// [`drain_pre_write_events`] and [`crate::state::wait_for_prompt_submission`],
/// which terminate the delivery before the next write is reached. The residual
/// window is a rollover landing between the end of a watch window and the write
/// that immediately follows it in the same task — sub-millisecond, and closable
/// only by threading the daemon's `AppState` into the spawn primitive.
///
/// The whole call is bounded by the shared `deadline` (B9). A wedged PTY can
/// still block inside the synchronous `write_all` under the writer mutex — that
/// is pre-existing behaviour of every write on this path and is tracked as a
/// follow-up, not fixed here — but the timeout does bound the far more common
/// case of waiting behind another writer.
async fn guarded_submit(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    agent_id: &str,
    prompt: &str,
    deadline: Instant,
) -> GuardedOutcome {
    let closing = Arc::clone(registry);
    // Issue #424 H5: the DETAILED form, because this is the path that owes the
    // user a terminal report and cannot produce one from a flattened `Stale`.
    let send = registry.write_and_submit_guarded_detailed(
        pane_id,
        prompt,
        Some(agent_id),
        || async move { !closing.is_pane_closing(pane_id) },
    );
    match tokio::time::timeout(remaining_before(deadline), send).await {
        Err(_) => GuardedOutcome::Refused("deadline elapsed while writing"),
        Ok(Err(e)) => GuardedOutcome::Failed(e),
        Ok(Ok(GuardedSendDetail::RefusedUserInput)) => GuardedOutcome::RefusedUserInput,
        Ok(Ok(GuardedSendDetail::Outcome(outcome))) => match outcome {
            GuardedSend::Applied => GuardedOutcome::Written,
            GuardedSend::WrongSession => GuardedOutcome::Refused("agent-replaced"),
            GuardedSend::Stale => GuardedOutcome::Refused("target went stale"),
            GuardedSend::NoLiveTarget => GuardedOutcome::Refused("no live target"),
            GuardedSend::Ambiguous => GuardedOutcome::Refused("ambiguous partial write"),
        },
    }
}

/// Consume everything already queued on `rx` — every frame of it produced
/// before the write this precedes — latching the pane's hook generation and
/// noting whether the producer can report a submitted prompt. Returns a stop
/// reason when the drained frames show the target is already gone.
///
/// Auditor HIGH: the generation decision is [`crate::state::latch_generation`],
/// the SAME function the post-write watch uses, rather than the open-coded copy
/// that used to live here. That copy never inspected `event_type`, so it happily
/// bound (or advanced to) a `SessionEnd` instead of stopping on it — a drained
/// end of the conversation we were about to write into looked like an ordinary
/// generation. One policy, one place: a future change to what is terminal cannot
/// apply to only one of the two.
///
/// Called TWICE per attempt on purpose (see [`confirm_prompt_delivery`]): once
/// before the first write, and again in the gap between a watch window expiring
/// and the retry that follows it. That gap used to be unguarded, so an end/start
/// pair arriving inside it was observed only AFTER the stale retry had landed.
fn drain_pre_write_events(
    rx: &mut broadcast::Receiver<BroadcastMsg>,
    pane_id: &str,
    agent_id: &str,
    generation: &mut Option<(String, DateTime<Utc>)>,
    can_report_prompts: &mut bool,
) -> Option<&'static str> {
    loop {
        match rx.try_recv() {
            Ok(BroadcastMsg::Event(event)) => {
                if event.pane_id.as_deref() != Some(pane_id) {
                    continue;
                }
                match event.agent_id.as_deref() {
                    Some(reported) if reported != agent_id => return Some("agent-replaced"),
                    None => continue,
                    Some(_) => {}
                }
                if crate::prompt_delivery::agent_reports_submitted_prompt(&event.agent_type) {
                    *can_report_prompts = true;
                }
                if let Some(crate::state::PromptWatch::TargetChanged { reason }) =
                    crate::state::latch_generation(generation, &event)
                {
                    return Some(reason);
                }
            }
            Ok(BroadcastMsg::OrchestrationSurface(_)) => continue,
            // Issue #424 D2 (both reviewers): TERMINAL, where this used to carry
            // on. The old comment claimed the dropped frames cost only the
            // generation latch "which the watcher re-establishes" — it does not.
            // `latch_generation` binds and re-binds on `SessionStart` alone, by
            // design, so if the only end/start transition for this pane fell out
            // of the broadcast ring the surviving ordinary frames announce
            // nothing and the delivery silently KEEPS a binding whose
            // conversation is gone. That is target-revocation evidence being
            // erased, which a same-user flood can arrange on purpose and ordinary
            // daemon load can arrange by accident.
            //
            // Post-write lag was already terminal in
            // `crate::state::wait_for_prompt_submission`; this is the same rule
            // at the two remaining drains. Before the first write it costs the
            // delivery entirely — the prompt is not written at all, logged under
            // this reason — which is a real availability cost, taken because a
            // 1024-frame ring overflowing inside the drain window is rare while
            // writing a spawn prompt into a conversation that may already have
            // been revoked is not recoverable.
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                return Some("lagged-event-stream");
            }
            Err(broadcast::error::TryRecvError::Empty) => return None,
            Err(broadcast::error::TryRecvError::Closed) => return Some("event-stream-closed"),
        }
    }
}

/// Issue #424: hold a spawn-time prompt PROVISIONAL until the agent reports
/// submitting it, re-submitting under a bounded backoff while it does not.
///
/// This is the daemon-side third delivery path — `dispatch`, the scheduler and
/// issue-dispatch all reach the PTY through [`deliver`], not through either of
/// the TUI-owned paths in `crate::ui` — and it is the one the reported failure
/// actually goes through: three `dispatch … --single` calls inside a minute, one
/// pane prompted and the other two healthy, in the right worktrees, and idle
/// forever with no prompt at all.
///
/// Every re-submission goes back through [`guarded_submit`], so it is a real
/// second write to the PTY *of the exact agent the first one reached* — never
/// of whoever happens to own the pane string by then.
///
/// Bounded by [`AUTOMATIC_PROMPT_DEADLINE`], after which the prompt is abandoned
/// with a warn, a durable report on the pane's card, and no further write. Every
/// retry types the prompt into the pane again, so the escalating
/// [`unconfirmed_retry_delay`] keeps that to single digits across the whole
/// window (see its docs).
///
/// `can_report_prompts` is the initial answer to "can this pane's delivery ever
/// be confirmed" — `true` when a pre-write event came from a producer that
/// reports submitted prompt text. It is not a fixed verdict: a pane that has
/// proved NOTHING yet is watched for one window first, and a later event from
/// such a producer arms retries from then on. That distinction keeps the two
/// populations apart without guessing from the command line:
///
/// * a bare shell / `cat` / a recorder stand-in — and, per reviewer finding B4,
///   a Pi pane, which emits well-formed status frames but structurally never a
///   submitted prompt — is never armed and receives exactly ONE write. Retrying
///   could not be recovery there, only the same prompt typed in again until the
///   deadline;
/// * a SLOW agent — the `devbox run claude …` launcher class from the issue,
///   whose readiness depends on a hook escaping a nested shell — signals late,
///   arms on that signal, and still gets its retries. Gating this on the
///   pre-write `SessionStart` alone would have denied a retry to exactly the
///   agents issue #424 §3 identifies as having the most fragile delivery.
async fn confirm_prompt_delivery(
    registry: Arc<AgentPtyRegistry>,
    mut rx: broadcast::Receiver<BroadcastMsg>,
    task: ConfirmationTask,
) {
    use crate::state::PromptWatch;

    let ConfirmationTask {
        pane_id,
        agent_id,
        prompt,
        delivery_id,
        mut generation,
        can_report_prompts,
        deadline,
    } = task;
    // Issue #424 S1/S2 (both reviewers): THIS delivery's own clock — the
    // delivery-scoped half of the user-draft rule, and the only half that needs
    // no shared bookkeeping at all: *a delivery stops retrying once user input
    // reaches its pane after its own write.* Nothing is keyed by pane, nothing
    // is keyed by bytes, nothing has to be released, and no other delivery can
    // disarm it. Sampled here rather than carried in [`ConfirmationTask`]
    // because this task is spawned immediately after the first write, so this
    // instant IS that write plus a scheduling hop; input landing inside that hop
    // is caught by the byte-keyed record below, whose timestamp is the write
    // itself. See `would_send_user_draft`.
    let watch_started_at = Instant::now();
    // Issue #424 H3 (both reviewers): this delivery OWNS the payload record its
    // first write left on the pane, and owns releasing it. However this task
    // ends — confirmed, accumulated, abandoned, target changed, lagged, closed,
    // refused, deadline — the record goes with it, so a later scheduled fire or
    // delegate hand-off carrying the SAME fixed text is a first write rather
    // than a repeat of a finished delivery's. That wrong refusal is #424 itself:
    // it lands before any byte is written, on a path whose only reaction is a
    // `warn!`. Released by `Drop` rather than at each of the eight exits,
    // because the one that gets forgotten is the one that reintroduces it.
    //
    // Issue #424 S2: ONE HOLDER PER PAYLOAD WRITE. A record is now per write
    // rather than per distinct payload, so that a delivery sharing its bytes
    // with a concurrent one cannot release the other's guard — which means this
    // delivery must hold (and drop) as many as it wrote. The replacement
    // payload's holder is pushed below, at the write that creates it.
    let mut payload_records = vec![PayloadRecordRelease {
        registry: &registry,
        pane_id: &pane_id,
        prompt: &prompt,
    }];
    let mut attempt: u32 = 1;
    let mut armed = can_report_prompts;
    // Issue #424 F4: whether a producer identifying itself AFTER the write may
    // arm this delivery at all. True only when something said BEFORE we wrote
    // what this pane is, so that a producer appearing later is genuinely this
    // delivery's target rather than an unauthenticated claim about a pane our
    // bytes have already gone into. Two things can say it:
    //
    // * the pane declared a LAUNCHER HANDOFF — the launcher consumed our bytes
    //   and the agent behind it is the authorized successor, the same single
    //   handoff `crate::state::latch_generation` permits for the generation;
    // * issue #570: THE DECK SPAWNED IT, as an agent type the deck itself
    //   selected. `dispatch --single` execs `default_command`, so on that path
    //   "is there an agent that reports submitted prompts in this pane" has a
    //   pre-write answer the deck WROTE rather than observed — and the gate was
    //   consulting neither of the two facts it had.
    //
    // Without the second, a daemon-spawned dispatch loses this delivery outright
    // whenever the agent's `SessionStart` misses the readiness gate: the field
    // report in #570 missed it by 37 ms, the write went out unarmed on the
    // fallback path, the producer announced itself 500 ms later, and the retry
    // that would have submitted the prompt was refused. That retry is not a
    // safety net on this path — the paired control in the same log shows attempt
    // 1 not submitting on the delivery that WORKED, which worked because it
    // retried — so refusing it is the difference between a dispatch and an agent
    // sitting in a fresh tab having been asked nothing.
    let accepts_late_producer = registry.agent_declared_launcher_handoff(&agent_id)
        || registry.agent_spawned_as_reporting_agent(&agent_id);
    let mut refused_claim_logged = false;
    loop {
        let remaining = remaining_before(deadline);
        if remaining.is_zero() {
            // Reviewer finding B3, daemon side: a delivery that never found a
            // producer capable of reporting a submitted prompt was not FAILED —
            // nothing could ever have confirmed it — so it is logged as
            // unconfirmable and reported nowhere. Reporting an error on a `cat`
            // pane's card would be a false alarm on every hookless delivery.
            if armed {
                abandon_spawn_prompt(
                    &registry,
                    &pane_id,
                    &agent_id,
                    &delivery_id,
                    attempt,
                    generation.as_ref(),
                );
            } else {
                log_prompt_unconfirmable(
                    DELIVERY_LOG_PATH,
                    &pane_id,
                    &delivery_id,
                    "this agent cannot report a submitted prompt, so nothing could confirm delivery",
                );
            }
            return;
        }
        let window = unconfirmed_retry_delay(attempt).min(remaining);
        match crate::state::wait_for_prompt_submission(
            &mut rx,
            &pane_id,
            &agent_id,
            &prompt,
            &mut generation,
            window,
        )
        .await
        {
            PromptWatch::Confirmed => {
                log_prompt_confirmed(DELIVERY_LOG_PATH, &pane_id, &delivery_id, attempt);
                return;
            }
            // Issue #424 D5: our prompt came back doubled with no separator, so
            // a payload had been sitting in the input box and a later write
            // appended to it. The agent has submitted it; a third copy is not
            // recovery. Terminal, and logged apart from a clean confirmation.
            PromptWatch::Accumulated => {
                crate::prompt_delivery::log_prompt_accumulated(
                    DELIVERY_LOG_PATH,
                    &pane_id,
                    &delivery_id,
                    attempt,
                );
                return;
            }
            // Issue #424 F4: a producer that identifies itself only after our
            // bytes were written has told us what it CLAIMS to be; it has not
            // told us that our bytes went to it. It arms this delivery only on
            // a pre-write statement about the pane — the pane's own launcher
            // declaration, or (issue #570) the deck's own record of having
            // spawned a known agent there. See `accepts_late_producer` above.
            // Refusals are logged once, so a delivery that then holds to its
            // deadline is diagnosable rather than mysterious.
            PromptWatch::Elapsed { can_report_prompts } => {
                armed |= can_report_prompts && accepts_late_producer;
                if can_report_prompts && !armed && !refused_claim_logged {
                    refused_claim_logged = true;
                    log_prompt_unconfirmable(
                        DELIVERY_LOG_PATH,
                        &pane_id,
                        &delivery_id,
                        "a producer claimed a reporting agent only after the prompt was written, \
                         and before it this pane neither declared a launcher handoff nor was \
                         spawned by this daemon as a known agent; holding the write instead of \
                         retyping into a target that may never report",
                    );
                }
            }
            // Reviewer findings B7/B8/B1: every one of these means the evidence
            // or the target is gone. Stop — do not read missing evidence as
            // permission to type the prompt again.
            PromptWatch::Indeterminate => {
                log_prompt_stopped(
                    DELIVERY_LOG_PATH,
                    &pane_id,
                    &delivery_id,
                    "lagged-event-stream",
                );
                return;
            }
            PromptWatch::Closed => {
                log_prompt_stopped(
                    DELIVERY_LOG_PATH,
                    &pane_id,
                    &delivery_id,
                    "event-stream-closed",
                );
                return;
            }
            PromptWatch::TargetChanged { reason } => {
                log_prompt_stopped(DELIVERY_LOG_PATH, &pane_id, &delivery_id, reason);
                return;
            }
        }
        // Reviewer finding B3, daemon side: capability is a property of the
        // PRODUCER, not a verdict a 500 ms timeout may return. Nothing has
        // identified itself yet, so the write stays PROVISIONAL — held, never
        // retyped — and the next window asks again. Returning here (what this
        // did) abandoned the watch half a second after the write while up to 59
        // seconds of the deadline remained, so an agent booting behind a
        // launcher had nobody left watching by the time it signalled: the exact
        // silent loss issue #424 reports, re-created by its own capability gate.
        // The cost is the mirror of the TUI's `ConfirmationCapability::Unknown`
        // — a genuinely hookless pane holds a (timer-only) watch until the
        // deadline instead of exiting after one window.
        if !armed {
            continue;
        }
        if remaining_before(deadline).is_zero() {
            abandon_spawn_prompt(
                &registry,
                &pane_id,
                &agent_id,
                &delivery_id,
                attempt,
                generation.as_ref(),
            );
            return;
        }
        // Auditor HIGH (the unguarded event gap): the watch window above stopped
        // reading the moment it expired, and everything between that instant and
        // the write below used to be observed only on the NEXT window — i.e.
        // after the stale retry had already landed. A `/clear` (or any
        // end/start pair) that lands in this gap is caught here instead, under
        // the same policy the window itself applies.
        // Issue #424 F4: the gap drain's capability observation is post-write
        // too, so it needs the same standing the window's does.
        let mut gap_capability = false;
        if let Some(reason) = drain_pre_write_events(
            &mut rx,
            &pane_id,
            &agent_id,
            &mut generation,
            &mut gap_capability,
        ) {
            log_prompt_stopped(DELIVERY_LOG_PATH, &pane_id, &delivery_id, reason);
            return;
        }
        armed |= gap_capability && accepts_late_producer;
        log_prompt_unconfirmed(DELIVERY_LOG_PATH, &pane_id, &delivery_id, attempt);
        attempt = attempt.saturating_add(1);
        // Issue #424 D5: after the one bounded replacement payload, later
        // attempts PROBE SUBMISSION instead of typing the prompt in again — same
        // guarded path, same writer serialization, same deadline, same
        // partial-write classification, only an empty payload so the target
        // receives just the delayed submit CR. See
        // [`crate::prompt_delivery::attempt_writes_payload`].
        let writes_payload = crate::prompt_delivery::attempt_writes_payload(attempt);
        // Issue #424 F1 (auditor HIGH): a probe submits whatever the target is
        // holding, so it is only meaningful while that is still OUR payload. The
        // registry refuses it outright once the user has typed since the last
        // automatic write — that check is the backstop for all three delivery
        // paths — but this loop asks first so the delivery stops for the reason
        // it actually stopped for, and REPORTS it on the pane's card. Without
        // the report this would be exactly the failure #424 is about: a written,
        // unconfirmed prompt that quietly stops being watched. There is nothing
        // else to try — retyping the payload into a pane someone is typing in is
        // strictly worse than stopping — so it is terminal, and the notice tells
        // the user their pane holds an automatic prompt they never submitted.
        //
        // Issue #424 F1, replacement-payload half: the SAME stop applies to the
        // one bounded replacement payload (attempt 2). Left unguarded it was the
        // worse half of the finding — the probe merely submits the user's draft,
        // whereas the replacement APPENDS our prompt to that draft and submits
        // the pair as a single turn. The registry asks the byte-keyed question
        // ("would this repeat what we already put in that box?"); this loop asks
        // the same one, for the same reason it asks the probe's, so the stop is
        // reported rather than surfacing as a bare `target went stale`.
        //
        // Issue #424 S1/S2 (both reviewers): the FIRST of the two questions
        // below is this delivery's OWN — has user input reached this pane since
        // MY first write — and it is the one that actually decides. It carries
        // no pane-keyed state, no digest, nothing to release and nothing another
        // delivery can disarm, so none of the shared-bookkeeping failures the
        // reviewers found (a paste falsely draining the records, a concurrent
        // same-byte delivery releasing this one's guard) can reach it. The
        // registry's byte-keyed question is kept as the SECOND line: it is the
        // only form of the rule the writer-held backstop can enforce for the two
        // TUI paths, whose deliveries live in a different process with no way to
        // hand their own clock across the wire. What this gives up is a retry
        // after the user submits their own turn — the byte-keyed record can tell
        // that the box was emptied, a timestamp cannot — and giving it up is the
        // safe direction: the delivery stops and REPORTS, with the prompt
        // visible in the box.
        let user_typed_since_our_own_write = registry
            .last_user_input_at(&pane_id)
            .is_some_and(|typed| typed > watch_started_at);
        let would_send_user_draft = user_typed_since_our_own_write
            || if writes_payload {
                registry.user_typed_since_writing_payload(&pane_id, &prompt)
            } else {
                registry.user_typed_since_automatic_write(&pane_id)
            };
        if would_send_user_draft {
            report_user_input_stop(
                &registry,
                &pane_id,
                &agent_id,
                &delivery_id,
                generation.as_ref(),
            );
            return;
        }
        let payload = if writes_payload { prompt.as_str() } else { "" };
        match guarded_submit(&registry, &pane_id, &agent_id, payload, deadline).await {
            GuardedOutcome::Written if writes_payload => {
                // Issue #424 S2: the replacement left a SECOND record of these
                // bytes on the pane. Take a holder for it here, at the write
                // that created it, so this delivery releases exactly what it
                // wrote and leaves nothing behind to refuse a later one.
                payload_records.push(PayloadRecordRelease {
                    registry: &registry,
                    pane_id: &pane_id,
                    prompt: &prompt,
                });
                log_prompt_written(DELIVERY_LOG_PATH, &pane_id, &delivery_id, attempt)
            }
            GuardedOutcome::Written => crate::prompt_delivery::log_prompt_probe_submitted(
                DELIVERY_LOG_PATH,
                &pane_id,
                &delivery_id,
                attempt,
            ),
            // Issue #424 H5: the user typed between the pre-check above and the
            // writer-held backstop. Same terminal outcome, same report — the
            // race the pre-check cannot cover is exactly the one the backstop
            // covers, and it must not be the one that reports nothing.
            GuardedOutcome::RefusedUserInput => {
                report_user_input_stop(
                    &registry,
                    &pane_id,
                    &agent_id,
                    &delivery_id,
                    generation.as_ref(),
                );
                return;
            }
            // The pane is gone (closed, exited, rebound) or the write must not
            // be repeated — stop rather than burn the deadline retrying.
            GuardedOutcome::Refused(reason) => {
                log_prompt_stopped(DELIVERY_LOG_PATH, &pane_id, &delivery_id, reason);
                return;
            }
            GuardedOutcome::Failed(e) => {
                tracing::warn!(
                    pane_id,
                    delivery_id,
                    error = %e,
                    "prompt re-submission failed; giving up on confirmation"
                );
                return;
            }
        }
    }
}

/// Issue #424 H3: releases ONE payload write's record when its confirmation task
/// ends, whichever of the many ways it ends. See the holders' construction in
/// [`confirm_prompt_delivery`].
///
/// Issue #424 S2: one holder per write, not per delivery. Records are per write,
/// so a delivery that wrote its payload twice holds two of these; and a delivery
/// sharing its bytes with a concurrent one releases only its own unit of guard
/// rather than disarming the other, which is what let a survivor's replacement
/// land on top of an unsent draft and submit both.
struct PayloadRecordRelease<'a> {
    registry: &'a Arc<AgentPtyRegistry>,
    pane_id: &'a str,
    prompt: &'a str,
}

impl Drop for PayloadRecordRelease<'_> {
    fn drop(&mut self) {
        self.registry
            .note_payload_settled(self.pane_id, self.prompt);
    }
}

/// Issue #424 F1/H5: stop this delivery because the pane's input box is no
/// longer ours to submit, and REPORT it on the pane's card.
///
/// Reached from three places that are the same stop seen at different instants:
/// the confirmation loop's own pre-check, the writer-held backstop's refusal a
/// few microseconds later, and the first detached write. One function so they
/// cannot drift into reporting differently — the backstop's silence was the
/// whole of the finding.
fn report_user_input_stop(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    agent_id: &str,
    delivery_id: &str,
    generation: Option<&(String, DateTime<Utc>)>,
) {
    log_prompt_stopped(
        DELIVERY_LOG_PATH,
        pane_id,
        delivery_id,
        "user-input-since-write",
    );
    registry.publish_delivery_notice(DeliveryNotice {
        pane_id: pane_id.to_string(),
        agent_id: agent_id.to_string(),
        delivery_id: delivery_id.to_string(),
        session_id: generation.map(|(id, _)| id.clone()),
        detail: "a spawn-time prompt was written into this pane and never confirmed, but \
                 you have typed into the pane since, so it was neither written again nor \
                 submitted for you; the prompt may still be sitting unsent in the agent's \
                 input box, above whatever you typed",
    });
}

/// Issue #424 §4 / reviewer finding on diagnosability: abandon an unconfirmed
/// spawn-time prompt LOUDLY.
///
/// The two TUI paths surface abandonment in the status bar, but this one is
/// detached and has no caller left by the time the deadline arrives, so a
/// `dispatch --single` whose prompt vanished was invisible in the default
/// environment — `init_logging_from_env` installs a subscriber only when
/// `DOT_AGENT_DECK_LOG` is set, which is exactly the diagnosability gap the
/// issue is about.
///
/// Reviewer blocker 3 / auditor MEDIUM: the report is DAEMON-SIDE STATE on the
/// pane's card, not bytes written into the agent's input buffer. The previous
/// round wrote a one-line notice through `write_notice_guarded`; that
/// primitive's own production contract says LF may be interpreted as Enter and
/// that a later ordinary submit sends `notice + newline + user prompt` as a
/// single turn (pinned by the passing
/// `write_to_pane_notice_bytes_precede_next_submit_with_only_lf_between`), so
/// into a pane that may already hold swallowed seed bytes the diagnostic could
/// itself become a task. PRD #249 is precedent for ACCEPTING that limitation in
/// the orchestrator's pane, not evidence it is safe here. See
/// [`DeliveryNotice`].
///
/// Now synchronous, which is the second half of reviewer finding B9: this used
/// to `await` a writer lock with no timeout AFTER the absolute deadline had
/// passed, so the registered task — and the cap slot it occupies — could outlive
/// the one deadline that was supposed to bound the whole delivery.
fn abandon_spawn_prompt(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    agent_id: &str,
    delivery_id: &str,
    attempts: u32,
    generation: Option<&(String, DateTime<Utc>)>,
) {
    log_prompt_abandoned(DELIVERY_LOG_PATH, pane_id, delivery_id, attempts);
    registry.publish_delivery_notice(DeliveryNotice {
        pane_id: pane_id.to_string(),
        agent_id: agent_id.to_string(),
        delivery_id: delivery_id.to_string(),
        session_id: generation.map(|(id, _)| id.clone()),
        detail: "a spawn-time prompt was written into this pane but the agent never reported \
                 submitting it within the delivery deadline; it may never have arrived — check \
                 whether this pane was given any task at all (the daemon log names the delivery \
                 id and the attempt count)",
    });
}

/// Everything one detached confirmation loop needs, bundled so the loop's
/// parameter list stays readable and the identity it is bound to travels as one
/// value.
struct ConfirmationTask {
    pane_id: String,
    agent_id: String,
    prompt: String,
    delivery_id: String,
    /// The pane's hook session as last observed, with the timestamp that
    /// established it. Latched pre-write and carried across every attempt so a
    /// `/clear` between them is caught (reviewer findings B1/B2).
    generation: Option<(String, DateTime<Utc>)>,
    can_report_prompts: bool,
    deadline: Instant,
}

/// Issue #424, reviewer finding B9 / auditor MEDIUM: the confirmation tasks
/// currently holding a spawn-time prompt provisional, keyed by pane id.
///
/// Before this there was no cancellation handle at all: a closed pane, a
/// rebound agent or a daemon shutdown was noticed only when a later write
/// happened to fail, and repeated dispatch into one pane could accumulate
/// tasks. This map gives all of that one home.
static CONFIRMATION_TASKS: std::sync::LazyLock<Mutex<HashMap<String, tokio::task::AbortHandle>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Ceiling on concurrently-live confirmation tasks across the daemon.
///
/// The map is keyed by pane and single-flight per pane, and each task is bounded
/// by [`AUTOMATIC_PROMPT_DEADLINE`], so this is not a work queue that can back
/// up: it is exceeded only when more than this many DISTINCT panes are inside
/// their delivery window at the same instant. That is a runaway backstop, and it
/// is kept.
///
/// What is NOT kept is what exhaustion used to mean (reviewer HIGH / auditor
/// LOW). The 257th prompt was written once and then simply not watched, with the
/// only trace an info-level log line — silent under the default no-subscriber
/// configuration — so the delivery that most needed #424's recovery was opted
/// out of it invisibly. It is reachable by CONFIGURATION rather than by
/// corruption: `issue_dispatch.max_per_run` has no upper bound and schedules can
/// overlap. Exhaustion is now reported on the pane's own card through the same
/// [`DeliveryNotice`] surface as an abandoned delivery, so "written but
/// unwatched" is visible exactly where "written and abandoned" is.
///
/// The number itself is deliberately unchanged. Raising it does not remove the
/// cliff, only move it; removing the cap trades a visible, bounded degradation
/// for an unbounded task pile. With exhaustion now visible, 256 concurrent
/// in-window panes is a threshold an operator can see and act on rather than a
/// silent policy.
const MAX_CONFIRMATION_TASKS: usize = 256;

/// Start the detached confirmation loop for one pane, under a PER-PANE
/// single-flight rule: a newer spawn-time prompt for the same pane cancels the
/// older watch rather than racing it. Two loops on one pane would type two
/// different prompts into it under two independent backoffs.
fn spawn_confirmation_task(
    registry: Arc<AgentPtyRegistry>,
    rx: broadcast::Receiver<BroadcastMsg>,
    task: ConfirmationTask,
) {
    let pane_id = task.pane_id.clone();
    let delivery_id = task.delivery_id.clone();
    let mut tasks = CONFIRMATION_TASKS.lock().unwrap();
    // Reap finished watches here rather than having each task deregister
    // itself: self-deregistration races its own registration (a fast task can
    // finish before the handle is filed) and would leak the entry it could not
    // find. `is_finished` needs no such coordination.
    tasks.retain(|_, handle| !handle.is_finished());
    if tasks.len() >= MAX_CONFIRMATION_TASKS && !tasks.contains_key(&pane_id) {
        drop(tasks);
        log_prompt_unconfirmable(
            DELIVERY_LOG_PATH,
            &pane_id,
            &delivery_id,
            "too many in-flight prompt confirmations; not watching this one",
        );
        // Reviewer HIGH: exhausting the cap silently opts the delivery out of
        // the recovery this whole issue is about. Report it where an abandoned
        // delivery is reported — see [`MAX_CONFIRMATION_TASKS`].
        registry.publish_delivery_notice(DeliveryNotice {
            pane_id: pane_id.clone(),
            agent_id: task.agent_id.clone(),
            delivery_id,
            session_id: task.generation.as_ref().map(|(id, _)| id.clone()),
            detail: "a spawn-time prompt was written into this pane but the daemon is already \
                     watching its maximum number of unconfirmed deliveries, so this one is NOT \
                     being confirmed or retried; check whether the pane acted on its task",
        });
        // Issue #424 H3 (reviewer's additional lifecycle gap): this delivery
        // ends HERE, before `confirm_prompt_delivery` — and therefore before its
        // RAII holder — ever exists, so the record its first write left has no
        // owner and would survive to the TTL, refusing an unrelated later
        // delivery of the same bytes. It will never be retried either, so
        // release it on the same terminal path that reports it.
        registry.note_payload_settled(&task.pane_id, &task.prompt);
        return;
    }
    // Held across `tokio::spawn`, which is synchronous — no await, so a `std`
    // mutex is safe here.
    let handle = tokio::spawn(confirm_prompt_delivery(registry, rx, task));
    if let Some(previous) = tasks.insert(pane_id, handle.abort_handle()) {
        previous.abort();
    }
}

/// Cancel the confirmation loop watching `pane_id`, if any. Called when the
/// pane closes: the prompt's target no longer exists, so neither the retries
/// nor the abandonment notice have anywhere to go.
pub fn cancel_prompt_confirmation(pane_id: &str) {
    if let Some(handle) = CONFIRMATION_TASKS.lock().unwrap().remove(pane_id) {
        handle.abort();
    }
}

/// Cancel every confirmation loop. Called on daemon shutdown, so a prompt watch
/// cannot outlive the daemon that owns the PTY it is writing into.
pub fn cancel_all_prompt_confirmations() {
    let handles: Vec<_> = CONFIRMATION_TASKS
        .lock()
        .unwrap()
        .drain()
        .map(|(_, handle)| handle)
        .collect();
    for handle in handles {
        handle.abort();
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
///
/// "Valid" means [`crate::agent_pty::is_valid_pane_id_env`] accepts it, and that
/// includes the [`PANE_ID_ENV_MAX_LEN`] byte cap — which this function used to
/// claim and not honour (issue #454). Schedule and dispatch task names have no
/// length bound of their own, so a long one produced an over-long id that
/// `AgentPtyRegistry::spawn_agent` refused to retain: the registry stored
/// `pane_id_env = None` while the child was launched with the full value, and
/// `ListAgents` could then never join the pane's live session onto its record.
/// That is issue #454's exact symptom (`daemon status` showing `STATUS=- TOOL=-`
/// and reconnect restoring `Idle`) surviving #454's fix, for every task whose
/// name is long enough. `StopAgent` could not recover the id either, so each
/// fire also leaked its per-pane daemon state.
///
/// The counter and role suffix carry the uniqueness, so it is the SANITIZED NAME
/// that gets truncated — never the suffix. Two long task names sharing a prefix
/// therefore produce ids that differ only in the counter, which is exactly what
/// the counter is for.
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
    let suffix = match role_index {
        Some(idx) => format!("-{n}-r{idx}"),
        None => format!("-{n}"),
    };
    // Every byte here is ASCII (the prefix is a literal, the counter and role
    // index are decimal, and the sanitizer maps every non-`[A-Za-z0-9_-]` char
    // to `-`), so a byte budget is also a char budget and truncating on it
    // cannot split a code point.
    let budget = crate::agent_pty::PANE_ID_ENV_MAX_LEN
        .saturating_sub(SCHEDULE_PANE_ID_PREFIX.len() + suffix.len());
    let sanitized = &sanitized[..sanitized.len().min(budget)];
    let pane_id = format!("{SCHEDULE_PANE_ID_PREFIX}{sanitized}{suffix}");
    // The budget can only underflow to zero if the fixed parts alone overflow
    // the cap, which needs a ~50-digit counter — unreachable for a `u64` this
    // process increments once per spawned pane. Asserted rather than truncated
    // because truncating the SUFFIX is the one repair that would be worse than
    // the problem: it is what makes two concurrent fires distinct.
    debug_assert!(
        crate::agent_pty::is_valid_pane_id_env(&pane_id),
        "next_pane_id must produce a valid DOT_AGENT_DECK_PANE_ID: {pane_id:?}"
    );
    pane_id
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
// One more than the lint's threshold, matching `spawn_one` above: these are the
// daemon-wide handles a fire needs (registry, reuse map, notifier, broadcast,
// AppState), each independently owned by the daemon. Bundling them into a struct
// purely to satisfy the count would add an indirection every call site has to
// build and no reader benefits from.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_or_reuse(
    req: SpawnRequest,
    new_tab_per_fire: bool,
    registry: &Arc<AgentPtyRegistry>,
    reuse: &ReuseRegistry,
    notifier: &dyn Notifier,
    debounce: Duration,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    state: Option<&crate::state::SharedState>,
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
            let handle = spawn(req, registry, notifier, event_tx, false, state).await?;
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use spec::spec;

    fn prompt_watch_event(
        pane_id: &str,
        agent_id: &str,
        session_id: &str,
        event_type: EventType,
    ) -> AgentEvent {
        AgentEvent {
            session_id: session_id.to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: Utc::now(),
            user_prompt: None,
            metadata: Default::default(),
            pane_id: Some(pane_id.to_string()),
            agent_id: Some(agent_id.to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        }
    }

    fn spawn_shell_target(registry: &Arc<AgentPtyRegistry>, pane_id: &str) -> String {
        let command = crate::platform::shell::fixed_command_shell("/bin/sh");
        registry
            .spawn_agent(SpawnOptions {
                command: Some(&command),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn shell observation target")
    }

    fn spawn_byte_target(registry: &Arc<AgentPtyRegistry>, pane_id: &str) -> String {
        spawn_typed_byte_target(registry, pane_id, None)
    }

    /// The same byte-observation target, spawned with the caller-supplied
    /// [`SpawnOptions::agent_type`] the deck itself decides at the spawn site
    /// (issue #570). `None` is the hookless pane the deck can vouch for
    /// nothing about; `Some(ClaudeCode)` is the `default_command = claude`
    /// dispatch the deck exec'd on purpose. The PTY is a byte sink either way,
    /// so the two differ in exactly the input under test.
    fn spawn_typed_byte_target(
        registry: &Arc<AgentPtyRegistry>,
        pane_id: &str,
        agent_type: Option<AgentType>,
    ) -> String {
        #[cfg(unix)]
        let command = "/bin/cat";
        #[cfg(windows)]
        let command = "more.com";

        registry
            .spawn_agent(SpawnOptions {
                command: Some(command),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
                agent_type,
                ..SpawnOptions::default()
            })
            .expect("spawn byte-observation target")
    }

    async fn type_user_bytes(
        registry: &AgentPtyRegistry,
        agent_id: &str,
        pane_id: &str,
        bytes: &[u8],
    ) {
        use std::io::Write as _;

        let handle = registry
            .subscribe(agent_id)
            .expect("attach detached byte-observation target");
        let mut writer = handle.writer.lock().await;
        writer
            .write_all(bytes)
            .expect("write detached user input bytes");
        writer.flush().expect("flush detached user input bytes");
        drop(writer);
        registry.note_user_input(pane_id);
        tokio::time::sleep(Duration::from_millis(75)).await;
    }

    async fn type_user_draft(
        registry: &AgentPtyRegistry,
        agent_id: &str,
        pane_id: &str,
        draft: &str,
    ) {
        type_user_bytes(registry, agent_id, pane_id, draft.as_bytes()).await;
    }

    struct UserFrameRetry {
        outcome: GuardedSend,
        before: Vec<u8>,
        after: Vec<u8>,
    }

    async fn retry_after_user_frame(
        pane_id: &str,
        prompt: &str,
        draft: &str,
        frame: &[u8],
    ) -> UserFrameRetry {
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = spawn_byte_target(&registry, pane_id);
        assert_eq!(
            registry
                .write_and_submit_guarded(pane_id, prompt, Some(&agent_id), || async { true })
                .await
                .expect("delivery before user newline control"),
            GuardedSend::Applied
        );
        type_user_draft(&registry, &agent_id, pane_id, draft).await;
        type_user_bytes(&registry, &agent_id, pane_id, frame).await;
        let before = registry
            .snapshot(&agent_id)
            .expect("before newline-control retry snapshot");
        assert!(
            before
                .windows(draft.len())
                .any(|window| window == draft.as_bytes()),
            "precondition: the unsent draft must physically reach the PTY before the retry; output={:?}",
            String::from_utf8_lossy(&before)
        );
        let outcome = registry
            .write_and_submit_guarded(pane_id, prompt, Some(&agent_id), || async { true })
            .await
            .expect("replacement after user newline control");
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after = registry
            .snapshot(&agent_id)
            .expect("after newline-control retry snapshot");
        registry.shutdown_all();
        UserFrameRetry {
            outcome,
            before,
            after,
        }
    }

    /// Issue #424 D2 (both reviewers): a lagged PRE-WRITE / gap drain is
    /// terminal.
    ///
    /// It used to `continue`, on the argument that the dropped frames cost only
    /// the generation latch "which the watcher re-establishes". It does not:
    /// `latch_generation` binds on a `SessionStart` alone, so if the only
    /// end/start transition for this pane fell out of the ring, the surviving
    /// ordinary frames announce nothing and the delivery keeps a binding whose
    /// conversation is gone. That is target-revocation evidence being erased —
    /// arrangeable on purpose by a same-user flood and by accident under load.
    #[test]
    fn a_lagged_pre_write_drain_is_terminal_rather_than_silently_continuing() {
        const PANE_ID: &str = "lagged-drain-pane";
        const AGENT_ID: &str = "lagged-drain-agent";

        let (tx, mut rx) = broadcast::channel(2);
        // Overflow the ring for this receiver, then leave one ordinary frame
        // behind it — the shape where the transition is gone and only
        // non-announcing frames remain.
        for i in 0..4 {
            let _ = tx.send(BroadcastMsg::Event(prompt_watch_event(
                PANE_ID,
                AGENT_ID,
                &format!("successor-{i}"),
                EventType::Thinking,
            )));
        }
        let mut generation = Some(("original-generation".to_string(), Utc::now()));
        let mut capability = false;
        assert_eq!(
            drain_pre_write_events(&mut rx, PANE_ID, AGENT_ID, &mut generation, &mut capability),
            Some("lagged-event-stream"),
            "dropped frames may have carried the end/start this delivery needed to see"
        );

        // A drain that loses nothing still returns `None`, so the ordinary path
        // is untouched.
        let (tx, mut rx) = broadcast::channel(8);
        let _ = tx.send(BroadcastMsg::Event(prompt_watch_event(
            PANE_ID,
            AGENT_ID,
            "original-generation",
            EventType::Thinking,
        )));
        let mut generation = Some(("original-generation".to_string(), Utc::now()));
        let mut capability = false;
        assert_eq!(
            drain_pre_write_events(&mut rx, PANE_ID, AGENT_ID, &mut generation, &mut capability),
            None
        );
        assert!(capability, "an identified Claude frame proves the channel");
    }

    /// Scenario: Hold detached spawn prompts in confirmation backoff while their pane is replaced, generation ends, event stream lags or closes, pane closes, daemon shuts down, a newer prompt supersedes the watch, or an unmarked event merely claims a reporting producer. Every terminal, cancelled, or unauthenticated-capability watch must finish without stale retry bytes. Finally, send that same unmarked post-write claim to a pane the deck itself spawned as a reporting agent: there it must arm the retry instead, so the prompt is typed into the pane again rather than held unsubmitted.
    #[spec("scheduler/dispatch/016")]
    #[serial_test::serial(prompt_confirmation_tasks)]
    #[tokio::test]
    async fn dispatch_016_detached_retry_stops_before_replacement_or_clear() {
        cancel_all_prompt_confirmations();
        const PROMPT: &str = "DETACHED-STALE-PROMPT-MARKER";
        const PANE_ID: &str = "detached-retry-rebind";

        let registry = Arc::new(AgentPtyRegistry::new());
        let original_id = spawn_shell_target(&registry, PANE_ID);
        let (event_tx, event_rx) = broadcast::channel(8);
        let confirmation = tokio::spawn(confirm_prompt_delivery(
            registry.clone(),
            event_rx,
            ConfirmationTask {
                pane_id: PANE_ID.to_string(),
                agent_id: original_id.clone(),
                prompt: PROMPT.to_string(),
                delivery_id: "replacement-guard-test".into(),
                generation: Some(("original-generation".into(), Utc::now())),
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));

        // The first confirmation window is the deterministic blocked retry.
        // Rebind while it is waiting, before the 500ms retry is resolved.
        tokio::time::sleep(Duration::from_millis(100)).await;
        registry
            .close_agent(&original_id)
            .expect("close original target");
        let replacement_id = spawn_shell_target(&registry, PANE_ID);
        tokio::time::timeout(Duration::from_secs(2), confirmation)
            .await
            .expect("replacement must terminate confirmation task")
            .expect("confirmation task must not panic");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let replacement_output = registry
            .snapshot(&replacement_id)
            .expect("replacement snapshot");
        assert!(
            !replacement_output
                .windows(PROMPT.len())
                .any(|window| window == PROMPT.as_bytes()),
            "a detached retry must send zero stale prompt bytes to a replacement agent; output={:?}",
            String::from_utf8_lossy(&replacement_output)
        );
        drop(event_tx);
        registry.shutdown_all();

        const CLEAR_PANE_ID: &str = "detached-retry-clear";
        let clear_registry = Arc::new(AgentPtyRegistry::new());
        let clear_agent_id = spawn_shell_target(&clear_registry, CLEAR_PANE_ID);
        let (clear_tx, clear_rx) = broadcast::channel(8);
        let clear_confirmation = tokio::spawn(confirm_prompt_delivery(
            clear_registry.clone(),
            clear_rx,
            ConfirmationTask {
                pane_id: CLEAR_PANE_ID.to_string(),
                agent_id: clear_agent_id.clone(),
                prompt: PROMPT.to_string(),
                delivery_id: "clear-generation-test".into(),
                generation: Some(("bound-before-clear".into(), Utc::now())),
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));
        clear_tx
            .send(BroadcastMsg::Event(prompt_watch_event(
                CLEAR_PANE_ID,
                &clear_agent_id,
                "bound-before-clear",
                EventType::SessionEnd,
            )))
            .expect("send bound SessionEnd");
        tokio::time::timeout(Duration::from_secs(1), clear_confirmation)
            .await
            .expect("SessionEnd must terminate confirmation task")
            .expect("clear confirmation task must not panic");
        tokio::time::sleep(Duration::from_millis(550)).await;
        let clear_output = clear_registry
            .snapshot(&clear_agent_id)
            .expect("same-agent snapshot");
        assert!(
            !clear_output
                .windows(PROMPT.len())
                .any(|window| window == PROMPT.as_bytes()),
            "SessionEnd for the bound generation must stop before retry bytes reach the cleared conversation"
        );
        clear_registry.shutdown_all();

        // A lagged stream may have dropped the real confirmation; a closed
        // stream can never report one. Both are terminal and neither permits a
        // retry after the ordinary 500 ms first window.
        let stream_registry = Arc::new(AgentPtyRegistry::new());
        let lagged_pane = "detached-retry-lagged";
        let lagged_agent = spawn_byte_target(&stream_registry, lagged_pane);
        let (lagged_tx, lagged_rx) = broadcast::channel(1);
        lagged_tx
            .send(BroadcastMsg::Event(prompt_watch_event(
                "other-pane-a",
                "other-agent",
                "other-session-a",
                EventType::Thinking,
            )))
            .expect("queue first event before lag");
        lagged_tx
            .send(BroadcastMsg::Event(prompt_watch_event(
                "other-pane-b",
                "other-agent",
                "other-session-b",
                EventType::Thinking,
            )))
            .expect("overflow confirmation receiver");
        let lagged_confirmation = tokio::spawn(confirm_prompt_delivery(
            stream_registry.clone(),
            lagged_rx,
            ConfirmationTask {
                pane_id: lagged_pane.into(),
                agent_id: lagged_agent.clone(),
                prompt: PROMPT.into(),
                delivery_id: "lagged-stream-test".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), lagged_confirmation)
            .await
            .expect("lagged stream must terminate the confirmation watch")
            .expect("lagged confirmation task must not panic");

        let closed_pane = "detached-retry-closed";
        let closed_agent = spawn_byte_target(&stream_registry, closed_pane);
        let (closed_tx, closed_rx) = broadcast::channel(1);
        drop(closed_tx);
        let closed_confirmation = tokio::spawn(confirm_prompt_delivery(
            stream_registry.clone(),
            closed_rx,
            ConfirmationTask {
                pane_id: closed_pane.into(),
                agent_id: closed_agent.clone(),
                prompt: PROMPT.into(),
                delivery_id: "closed-stream-test".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), closed_confirmation)
            .await
            .expect("closed stream must terminate instead of spinning")
            .expect("closed confirmation task must not panic");
        tokio::time::sleep(Duration::from_millis(550)).await;
        for (agent_id, terminal) in [
            (&lagged_agent, "Lagged must become terminal Indeterminate"),
            (&closed_agent, "Closed must remain terminal"),
        ] {
            let output = stream_registry.snapshot(agent_id).expect("stream snapshot");
            assert!(
                !output
                    .windows(PROMPT.len())
                    .any(|window| window == PROMPT.as_bytes()),
                "{terminal}; no retry bytes may follow: {:?}",
                String::from_utf8_lossy(&output)
            );
        }
        drop(lagged_tx);
        stream_registry.shutdown_all();

        // The registry-owned lifecycle: pane close aborts its watch, shutdown
        // aborts all watches, and a newer prompt for one pane aborts the older
        // flight before its first retry can land.
        let managed_registry = Arc::new(AgentPtyRegistry::new());
        let (managed_tx, _) = broadcast::channel(8);
        let close_pane = "detached-close-cancel";
        let close_agent = spawn_byte_target(&managed_registry, close_pane);
        spawn_confirmation_task(
            managed_registry.clone(),
            managed_tx.subscribe(),
            ConfirmationTask {
                pane_id: close_pane.into(),
                agent_id: close_agent.clone(),
                prompt: "CLOSE-CANCELLED-OLD-PROMPT".into(),
                delivery_id: "close-cancel-test".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        );
        cancel_prompt_confirmation(close_pane);

        let single_pane = "detached-single-flight";
        let single_agent = spawn_byte_target(&managed_registry, single_pane);
        spawn_confirmation_task(
            managed_registry.clone(),
            managed_tx.subscribe(),
            ConfirmationTask {
                pane_id: single_pane.into(),
                agent_id: single_agent.clone(),
                prompt: "SUPERSEDED-OLD-PROMPT".into(),
                delivery_id: "single-flight-old".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        );
        spawn_confirmation_task(
            managed_registry.clone(),
            managed_tx.subscribe(),
            ConfirmationTask {
                pane_id: single_pane.into(),
                agent_id: single_agent.clone(),
                prompt: "NEWER-PROMPT-HELD-WITHOUT-RETRY".into(),
                delivery_id: "single-flight-new".into(),
                generation: None,
                can_report_prompts: false,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        );

        let shutdown_panes = ["detached-shutdown-a", "detached-shutdown-b"];
        let shutdown_agents = shutdown_panes.map(|pane_id| {
            let agent_id = spawn_byte_target(&managed_registry, pane_id);
            spawn_confirmation_task(
                managed_registry.clone(),
                managed_tx.subscribe(),
                ConfirmationTask {
                    pane_id: pane_id.into(),
                    agent_id: agent_id.clone(),
                    prompt: format!("SHUTDOWN-CANCELLED-{pane_id}"),
                    delivery_id: format!("shutdown-cancel-{pane_id}"),
                    generation: None,
                    can_report_prompts: true,
                    deadline: Instant::now() + Duration::from_secs(3),
                },
            );
            agent_id
        });
        cancel_all_prompt_confirmations();
        tokio::time::sleep(Duration::from_millis(550)).await;

        let close_output = managed_registry
            .snapshot(&close_agent)
            .expect("pane-close cancellation snapshot");
        assert!(
            !String::from_utf8_lossy(&close_output).contains("CLOSE-CANCELLED-OLD-PROMPT"),
            "pane close must abort its confirmation task before retry bytes"
        );
        let single_output = managed_registry
            .snapshot(&single_agent)
            .expect("single-flight snapshot");
        assert!(
            !String::from_utf8_lossy(&single_output).contains("SUPERSEDED-OLD-PROMPT"),
            "the newer same-pane watch must abort the older prompt before it retries"
        );
        for (pane_id, agent_id) in shutdown_panes.iter().zip(shutdown_agents.iter()) {
            let output = managed_registry
                .snapshot(agent_id)
                .expect("daemon-shutdown cancellation snapshot");
            assert!(
                !String::from_utf8_lossy(&output).contains("SHUTDOWN-CANCELLED"),
                "daemon shutdown must abort the watch for {pane_id} before retry bytes"
            );
        }
        assert!(
            CONFIRMATION_TASKS.lock().unwrap().is_empty(),
            "daemon shutdown must drain the confirmation task registry"
        );
        drop(managed_tx);
        managed_registry.shutdown_all();

        // Auditor E4: the target is a hookless byte sink. A producer-controlled
        // event that merely declares a reporting AgentType, with no
        // `wrapper_fork` marker to trip the narrow exclusion, must not arm the
        // detached retry loop.
        const FORGED_PANE: &str = "unmarked-forged-detached-pane";
        const FORGED_PROMPT: &str = "UNMARKED-FORGED-RETRY-MUST-NOT-LAND";
        let forged_registry = Arc::new(AgentPtyRegistry::new());
        let forged_agent = spawn_byte_target(&forged_registry, FORGED_PANE);
        let (forged_tx, forged_rx) = broadcast::channel(8);
        let forged_confirmation = tokio::spawn(confirm_prompt_delivery(
            forged_registry.clone(),
            forged_rx,
            ConfirmationTask {
                pane_id: FORGED_PANE.into(),
                agent_id: forged_agent.clone(),
                prompt: FORGED_PROMPT.into(),
                delivery_id: "unmarked-forged-capability".into(),
                generation: None,
                can_report_prompts: false,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));
        forged_tx
            .send(BroadcastMsg::Event(prompt_watch_event(
                FORGED_PANE,
                &forged_agent,
                "forged-unmarked-session",
                EventType::SessionStart,
            )))
            .expect("send unmarked forged capability claim");
        tokio::time::sleep(Duration::from_millis(750)).await;
        let forged_output = forged_registry
            .snapshot(&forged_agent)
            .expect("forged capability target snapshot");
        forged_confirmation.abort();
        let _ = forged_confirmation.await;
        drop(forged_tx);
        forged_registry.shutdown_all();
        assert!(
            !forged_output
                .windows(FORGED_PROMPT.len())
                .any(|window| window == FORGED_PROMPT.as_bytes()),
            "an unmarked producer assertion must not arm a full replacement payload on a hookless target; output={:?}",
            String::from_utf8_lossy(&forged_output)
        );

        // Issue #570: the SAME late unmarked claim, on a pane the DECK ITSELF
        // spawned with an agent type IT chose — `default_command = claude`, so
        // `SpawnOptions::agent_type` said ClaudeCode before a byte was written.
        // That is a pre-write declaration by the deck, not a producer
        // assertion, and it is the standing the forged case above lacks. The
        // two panes differ in exactly that one input: same `/bin/cat` byte
        // sink, same `can_report_prompts: false`, same unmarked post-write
        // `SessionStart`. Without it a daemon-spawned dispatch whose
        // `SessionStart` lands after the readiness gate expired is written and
        // never submitted — no retry ever fires, so nothing types the payload
        // the agent would have to submit.
        const SPAWNED_PANE: &str = "deck-spawned-late-claim-pane";
        const SPAWNED_PROMPT: &str = "DECK-SPAWNED-LATE-CLAIM-MUST-STILL-RETRY";
        let spawned_registry = Arc::new(AgentPtyRegistry::new());
        let spawned_agent =
            spawn_typed_byte_target(&spawned_registry, SPAWNED_PANE, Some(AgentType::ClaudeCode));
        let (spawned_tx, spawned_rx) = broadcast::channel(8);
        let spawned_confirmation = tokio::spawn(confirm_prompt_delivery(
            spawned_registry.clone(),
            spawned_rx,
            ConfirmationTask {
                pane_id: SPAWNED_PANE.into(),
                agent_id: spawned_agent.clone(),
                prompt: SPAWNED_PROMPT.into(),
                delivery_id: "deck-spawned-late-capability".into(),
                generation: None,
                can_report_prompts: false,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));
        spawned_tx
            .send(BroadcastMsg::Event(prompt_watch_event(
                SPAWNED_PANE,
                &spawned_agent,
                "late-native-session",
                EventType::SessionStart,
            )))
            .expect("send late native capability claim");
        tokio::time::sleep(Duration::from_millis(750)).await;
        let spawned_output = spawned_registry
            .snapshot(&spawned_agent)
            .expect("deck-spawned target snapshot");
        spawned_confirmation.abort();
        let _ = spawned_confirmation.await;
        drop(spawned_tx);
        spawned_registry.shutdown_all();
        assert!(
            spawned_output
                .windows(SPAWNED_PROMPT.len())
                .any(|window| window == SPAWNED_PROMPT.as_bytes()),
            "a producer identifying itself after the write must still arm the retry on a pane the deck spawned as a reporting agent, or the dispatch prompt is written and never submitted (#570); output={:?}",
            String::from_utf8_lossy(&spawned_output)
        );
    }

    /// Scenario: Deliver a detached spawn prompt, type an unsent user draft before the replacement payload is due, and independently type another draft after the replacement but before the submit-only probe. In both timelines the next automatic attempt must send no bytes, so it neither appends its payload nor submits the user's draft.
    #[spec("scheduler/dispatch/018")]
    #[tokio::test]
    async fn dispatch_018_user_input_disarms_detached_submit_probe() {
        const PANE_ID: &str = "detached-user-draft-pane";
        const PROMPT: &str = "AUTOMATIC-PROMPT-BEFORE-USER-DRAFT";
        const USER_DRAFT: &str = "detached draft deliberately left unsent";

        const REPLACEMENT_PANE_ID: &str = "detached-draft-before-replacement-pane";
        const REPLACEMENT_PROMPT: &str = "AUTOMATIC-PROMPT-BEFORE-REPLACEMENT-GUARD";
        const REPLACEMENT_DRAFT: &str = "detached draft before replacement payload";

        let replacement_registry = Arc::new(AgentPtyRegistry::new());
        let replacement_agent = spawn_byte_target(&replacement_registry, REPLACEMENT_PANE_ID);
        let initial = replacement_registry
            .write_and_submit_guarded(
                REPLACEMENT_PANE_ID,
                REPLACEMENT_PROMPT,
                Some(&replacement_agent),
                || async { true },
            )
            .await
            .expect("attempt 1 guarded delivery");
        assert_eq!(
            initial,
            GuardedSend::Applied,
            "attempt 1 must never be refused before an automatic write timestamp exists"
        );
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after_initial_delivery = replacement_registry
            .snapshot(&replacement_agent)
            .expect("initial detached delivery snapshot");
        assert!(
            after_initial_delivery
                .windows(REPLACEMENT_PROMPT.len())
                .any(|window| window == REPLACEMENT_PROMPT.as_bytes()),
            "precondition: attempt 1 must physically reach the detached pane; output={:?}",
            String::from_utf8_lossy(&after_initial_delivery)
        );

        type_user_draft(
            &replacement_registry,
            &replacement_agent,
            REPLACEMENT_PANE_ID,
            REPLACEMENT_DRAFT,
        )
        .await;
        let before_replacement = replacement_registry
            .snapshot(&replacement_agent)
            .expect("pre-replacement detached snapshot");
        assert!(
            before_replacement
                .windows(REPLACEMENT_DRAFT.len())
                .any(|window| window == REPLACEMENT_DRAFT.as_bytes()),
            "precondition: the unsent user draft must physically reach the detached PTY; output={:?}",
            String::from_utf8_lossy(&before_replacement)
        );

        let (replacement_tx, replacement_rx) = broadcast::channel(8);
        let replacement_confirmation = tokio::spawn(confirm_prompt_delivery(
            replacement_registry.clone(),
            replacement_rx,
            ConfirmationTask {
                pane_id: REPLACEMENT_PANE_ID.into(),
                agent_id: replacement_agent.clone(),
                prompt: REPLACEMENT_PROMPT.into(),
                delivery_id: "detached-replacement-user-draft-safety".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));
        tokio::time::sleep(Duration::from_millis(800)).await;
        let after_replacement = replacement_registry
            .snapshot(&replacement_agent)
            .expect("post-replacement detached snapshot");

        replacement_confirmation.abort();
        let _ = replacement_confirmation.await;
        drop(replacement_tx);
        replacement_registry.shutdown_all();
        assert_eq!(
            after_replacement,
            before_replacement,
            "detached attempt 2 must append no replacement payload and send no submit CR after user input; before={:?}, after={:?}",
            String::from_utf8_lossy(&before_replacement),
            String::from_utf8_lossy(&after_replacement)
        );

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = spawn_byte_target(&registry, PANE_ID);
        let (event_tx, event_rx) = broadcast::channel(8);
        let confirmation = tokio::spawn(confirm_prompt_delivery(
            registry.clone(),
            event_rx,
            ConfirmationTask {
                pane_id: PANE_ID.into(),
                agent_id: agent_id.clone(),
                prompt: PROMPT.into(),
                delivery_id: "detached-user-draft-safety".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(4),
            },
        ));

        tokio::time::sleep(Duration::from_millis(800)).await;
        let before_user_input = registry
            .snapshot(&agent_id)
            .expect("replacement payload snapshot");
        assert!(
            before_user_input
                .windows(PROMPT.len())
                .any(|window| window == PROMPT.as_bytes()),
            "precondition: attempt 2 must have reached the byte target before the user types"
        );

        type_user_draft(&registry, &agent_id, PANE_ID, USER_DRAFT).await;
        let before_probe = registry
            .snapshot(&agent_id)
            .expect("pre-probe pane snapshot");
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let after_probe = registry
            .snapshot(&agent_id)
            .expect("post-probe pane snapshot");

        confirmation.abort();
        let _ = confirmation.await;
        drop(event_tx);
        registry.shutdown_all();
        assert_eq!(
            after_probe,
            before_probe,
            "a detached retry must send no submit CR after user input was recorded; before={:?}, after={:?}",
            String::from_utf8_lossy(&before_probe),
            String::from_utf8_lossy(&after_probe)
        );
    }

    /// Scenario: Queue an automatic replacement behind the same writer that is forwarding an attached user's unsent draft, then release the writer without stamping the user-input clock. The queued retry must not append or submit anything before that clock stamp can run.
    #[spec("scheduler/dispatch/019")]
    #[tokio::test]
    async fn dispatch_019_writer_release_does_not_expose_unstamped_user_input() {
        use std::io::Write as _;

        const PANE_ID: &str = "writer-release-clock-race-pane";
        const PROMPT: &str = "automatic payload already delivered once";
        const USER_DRAFT: &str = "attached user draft left unsent";

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = spawn_byte_target(&registry, PANE_ID);
        assert_eq!(
            registry
                .write_and_submit_guarded(PANE_ID, PROMPT, Some(&agent_id), || async { true })
                .await
                .expect("initial guarded delivery"),
            GuardedSend::Applied
        );

        let handle = registry
            .subscribe(&agent_id)
            .expect("attach byte-observation target");
        let mut user_writer = handle.writer.lock().await;
        let retry_registry = registry.clone();
        let retry_agent = agent_id.clone();
        let retry = tokio::spawn(async move {
            retry_registry
                .write_and_submit_guarded(PANE_ID, PROMPT, Some(&retry_agent), || async { true })
                .await
                .expect("queued guarded replacement")
        });
        tokio::task::yield_now().await;
        assert!(
            !retry.is_finished(),
            "precondition: the replacement must be queued behind the attached user's writer"
        );

        user_writer
            .write_all(USER_DRAFT.as_bytes())
            .expect("forward unsent attached user draft");
        user_writer
            .flush()
            .expect("flush unsent attached user draft");
        tokio::time::sleep(Duration::from_millis(75)).await;
        let before_writer_release = registry.snapshot(&agent_id).expect("pre-release snapshot");
        assert!(
            before_writer_release
                .windows(USER_DRAFT.len())
                .any(|window| window == USER_DRAFT.as_bytes()),
            "precondition: the user's unsent bytes must already be physically visible before the clock stamp; output={:?}",
            String::from_utf8_lossy(&before_writer_release)
        );

        // Exact production ordering under test: STREAM_IN drops the pane writer,
        // then stamps `user_input_at`. Awaiting the already-queued replacement
        // before stamping forces it to own that handoff window; there is no
        // scheduler timing by which this fixture can stamp the clock first.
        drop(user_writer);
        let retry_outcome = retry.await.expect("queued replacement task");
        registry.note_user_input(PANE_ID);
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after_clock_stamp = registry.snapshot(&agent_id).expect("post-race snapshot");

        registry.shutdown_all();
        assert_eq!(
            retry_outcome,
            GuardedSend::Stale,
            "a replacement that acquires the writer after user bytes but before their clock stamp must be refused"
        );
        assert_eq!(
            after_clock_stamp,
            before_writer_release,
            "the writer-to-clock handoff must not let a retry append its payload or submit the user's draft; before={:?}, after={:?}",
            String::from_utf8_lossy(&before_writer_release),
            String::from_utf8_lossy(&after_clock_stamp)
        );
    }

    /// Scenario: Exercise later, overlapping, and retried automatic deliveries against real byte-observation PTYs, including bracketed paste, production-encoded Ctrl+J and Alt+Enter newlines, plain Enter, and two active same-text owners. Completed work and a submitted turn may admit a later write, but non-submitting editor controls must leave an automatic retry unable to append to or submit the user's draft.
    #[spec("scheduler/dispatch/020")]
    #[tokio::test]
    async fn dispatch_020_payload_guards_are_scoped_to_one_delivery() {
        const SAME_PANE: &str = "later-same-payload-pane";
        const SAME_PROMPT: &str = "fixed worker task pointer";

        let same_registry = Arc::new(AgentPtyRegistry::new());
        let same_agent = spawn_byte_target(&same_registry, SAME_PANE);
        assert_eq!(
            same_registry
                .write_and_submit_guarded(SAME_PANE, SAME_PROMPT, Some(&same_agent), || async {
                    true
                })
                .await
                .expect("delivery A"),
            GuardedSend::Applied
        );
        type_user_draft(
            &same_registry,
            &same_agent,
            SAME_PANE,
            "user completed an unrelated turn\r",
        )
        .await;
        let before_delivery_b = same_registry
            .snapshot(&same_agent)
            .expect("before delivery B snapshot");
        let delivery_b = same_registry
            .write_and_submit_guarded(SAME_PANE, SAME_PROMPT, Some(&same_agent), || async { true })
            .await
            .expect("delivery B first attempt");
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after_delivery_b = same_registry
            .snapshot(&same_agent)
            .expect("after delivery B snapshot");

        const REPLACED_PANE: &str = "different-submit-replaces-digest-pane";
        const DELIVERY_A: &str = "older delivery payload A";
        const DELIVERY_B: &str = "independent delivery payload B";
        let replaced_registry = Arc::new(AgentPtyRegistry::new());
        let replaced_agent = spawn_byte_target(&replaced_registry, REPLACED_PANE);
        assert_eq!(
            replaced_registry
                .write_and_submit_guarded(
                    REPLACED_PANE,
                    DELIVERY_A,
                    Some(&replaced_agent),
                    || async { true },
                )
                .await
                .expect("delivery A first attempt"),
            GuardedSend::Applied
        );
        type_user_draft(
            &replaced_registry,
            &replaced_agent,
            REPLACED_PANE,
            "draft that invalidates delivery A",
        )
        .await;
        assert_eq!(
            replaced_registry
                .write_and_submit_guarded(
                    REPLACED_PANE,
                    DELIVERY_B,
                    Some(&replaced_agent),
                    || async { true },
                )
                .await
                .expect("independent delivery B"),
            GuardedSend::Applied,
            "precondition: the different guarded submit must replace the pane-global payload slot"
        );
        tokio::time::sleep(Duration::from_millis(75)).await;
        let before_delivery_a_retry = replaced_registry
            .snapshot(&replaced_agent)
            .expect("before delivery A retry snapshot");
        let delivery_a_retry = replaced_registry
            .write_and_submit_guarded(REPLACED_PANE, DELIVERY_A, Some(&replaced_agent), || async {
                true
            })
            .await
            .expect("delivery A replacement");
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after_delivery_a_retry = replaced_registry
            .snapshot(&replaced_agent)
            .expect("after delivery A retry snapshot");

        const PASTE_PANE: &str = "bracketed-multiline-draft-pane";
        const PASTE_PROMPT: &str = "automatic payload before bracketed paste";
        const BRACKETED_DRAFT: &str =
            "\x1b[200~first line of an unsent draft\nsecond line of an unsent draft\x1b[201~";
        let paste_registry = Arc::new(AgentPtyRegistry::new());
        let paste_agent = spawn_byte_target(&paste_registry, PASTE_PANE);
        assert_eq!(
            paste_registry
                .write_and_submit_guarded(PASTE_PANE, PASTE_PROMPT, Some(&paste_agent), || async {
                    true
                },)
                .await
                .expect("delivery before bracketed paste"),
            GuardedSend::Applied
        );
        type_user_draft(&paste_registry, &paste_agent, PASTE_PANE, BRACKETED_DRAFT).await;
        let before_paste_retry = paste_registry
            .snapshot(&paste_agent)
            .expect("before bracketed-paste retry snapshot");
        assert!(
            before_paste_retry
                .windows(b"first line of an unsent draft".len())
                .any(|window| window == b"first line of an unsent draft")
                && before_paste_retry
                    .windows(b"second line".len())
                    .any(|window| window == b"second line"),
            "precondition: the production-shaped bracketed multiline paste must physically reach the PTY; output={:?}",
            String::from_utf8_lossy(&before_paste_retry)
        );
        let paste_retry = paste_registry
            .write_and_submit_guarded(PASTE_PANE, PASTE_PROMPT, Some(&paste_agent), || async {
                true
            })
            .await
            .expect("replacement after bracketed paste");
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after_paste_retry = paste_registry
            .snapshot(&paste_agent)
            .expect("after bracketed-paste retry snapshot");

        let ctrl_j_frame = crate::ui::keyevent_to_bytes_for_test(&KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        ))
        .expect("production Ctrl+J encoding");
        let ctrl_j_retry = retry_after_user_frame(
            "ctrl-j-unsent-draft-pane",
            "automatic payload before Ctrl+J",
            "draft extended with a Ctrl+J newline",
            &ctrl_j_frame,
        )
        .await;

        let alt_enter_frame = crate::ui::keyevent_to_bytes_for_test(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        ))
        .expect("production Alt+Enter encoding");
        let alt_enter_retry = retry_after_user_frame(
            "alt-enter-unsent-draft-pane",
            "automatic payload before Alt+Enter",
            "Claude draft extended with an Alt+Enter newline",
            &alt_enter_frame,
        )
        .await;

        let plain_enter_frame = crate::ui::keyevent_to_bytes_for_test(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))
        .expect("production plain Enter encoding");
        let plain_enter_retry = retry_after_user_frame(
            "plain-enter-submitted-pane",
            "automatic payload before plain Enter",
            "user turn completed with plain Enter",
            &plain_enter_frame,
        )
        .await;
        assert!(
            plain_enter_retry.outcome == GuardedSend::Applied
                && plain_enter_retry.after.len() > plain_enter_retry.before.len(),
            "positive control: a genuine plain Enter must drain the completed turn and admit the later automatic payload; outcome={:?}, before={:?}, after={:?}",
            plain_enter_retry.outcome,
            String::from_utf8_lossy(&plain_enter_retry.before),
            String::from_utf8_lossy(&plain_enter_retry.after)
        );

        const OVERLAP_PANE: &str = "overlapping-same-payload-pane";
        const OVERLAP_PROMPT: &str = "same payload owned by two active deliveries";
        const OVERLAP_DRAFT: &str = "user draft after delivery A was superseded";
        let overlap_registry = Arc::new(AgentPtyRegistry::new());
        let overlap_agent = spawn_byte_target(&overlap_registry, OVERLAP_PANE);
        assert_eq!(
            overlap_registry
                .write_and_submit_guarded(
                    OVERLAP_PANE,
                    OVERLAP_PROMPT,
                    Some(&overlap_agent),
                    || async { true },
                )
                .await
                .expect("overlapping delivery A first write"),
            GuardedSend::Applied
        );
        let delivery_a_release = PayloadRecordRelease {
            registry: &overlap_registry,
            pane_id: OVERLAP_PANE,
            prompt: OVERLAP_PROMPT,
        };
        assert_eq!(
            overlap_registry
                .write_and_submit_guarded(
                    OVERLAP_PANE,
                    OVERLAP_PROMPT,
                    Some(&overlap_agent),
                    || async { true },
                )
                .await
                .expect("overlapping delivery B first write"),
            GuardedSend::Applied
        );
        let delivery_b_release = PayloadRecordRelease {
            registry: &overlap_registry,
            pane_id: OVERLAP_PANE,
            prompt: OVERLAP_PROMPT,
        };
        // The production single-flight replacement aborts A after B has
        // refreshed its same-byte write. Dropping A's real release owner here
        // forces that ordering without leaving it to task-scheduler timing.
        drop(delivery_a_release);
        type_user_draft(
            &overlap_registry,
            &overlap_agent,
            OVERLAP_PANE,
            OVERLAP_DRAFT,
        )
        .await;
        let before_overlap_retry = overlap_registry
            .snapshot(&overlap_agent)
            .expect("before surviving delivery retry snapshot");
        let overlap_retry = overlap_registry
            .write_and_submit_guarded(
                OVERLAP_PANE,
                OVERLAP_PROMPT,
                Some(&overlap_agent),
                || async { true },
            )
            .await
            .expect("surviving delivery B replacement");
        tokio::time::sleep(Duration::from_millis(75)).await;
        let after_overlap_retry = overlap_registry
            .snapshot(&overlap_agent)
            .expect("after surviving delivery retry snapshot");
        drop(delivery_b_release);

        same_registry.shutdown_all();
        replaced_registry.shutdown_all();
        paste_registry.shutdown_all();
        overlap_registry.shutdown_all();
        assert!(
            delivery_b == GuardedSend::Applied
                && after_delivery_b.len() > before_delivery_b.len()
                && delivery_a_retry == GuardedSend::Stale
                && after_delivery_a_retry == before_delivery_a_retry
                && after_paste_retry == before_paste_retry
                && ctrl_j_retry.after == ctrl_j_retry.before
                && alt_enter_retry.after == alt_enter_retry.before
                && after_overlap_retry == before_overlap_retry,
            "payload safety must be scoped by logical delivery and preserve unsent drafts: later_same_payload={{outcome: {delivery_b:?}, before_len: {}, after_len: {}}}; older_retry_after_different_submit={{outcome: {delivery_a_retry:?}, before_len: {}, after_len: {}}}; bracketed_multiline_paste={{outcome: {paste_retry:?}, before: {:?}, after: {:?}}}; ctrl_j_newline={{outcome: {:?}, before: {:?}, after: {:?}}}; alt_enter_newline={{outcome: {:?}, before: {:?}, after: {:?}}}; overlapping_same_payload={{outcome: {overlap_retry:?}, before: {:?}, after: {:?}}}",
            before_delivery_b.len(),
            after_delivery_b.len(),
            before_delivery_a_retry.len(),
            after_delivery_a_retry.len(),
            String::from_utf8_lossy(&before_paste_retry),
            String::from_utf8_lossy(&after_paste_retry),
            ctrl_j_retry.outcome,
            String::from_utf8_lossy(&ctrl_j_retry.before),
            String::from_utf8_lossy(&ctrl_j_retry.after),
            alt_enter_retry.outcome,
            String::from_utf8_lossy(&alt_enter_retry.before),
            String::from_utf8_lossy(&alt_enter_retry.after),
            String::from_utf8_lossy(&before_overlap_retry),
            String::from_utf8_lossy(&after_overlap_retry)
        );
    }

    /// Scenario: Hold the pane writer while the detached confirmation loop finishes its user-input precheck, then record user input before releasing the writer to its guarded backstop. The resulting refusal must publish a delivery notice instead of ending as a log-only stop.
    #[spec("scheduler/dispatch/021")]
    #[tokio::test]
    async fn dispatch_021_backstop_user_input_refusal_is_reported() {
        use std::io::Write as _;

        const PANE_ID: &str = "detached-backstop-report-pane";
        const PROMPT: &str = "detached prompt awaiting confirmation";
        const USER_DRAFT: &str = "draft arriving after caller precheck";

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = spawn_byte_target(&registry, PANE_ID);
        assert_eq!(
            registry
                .write_and_submit_guarded(PANE_ID, PROMPT, Some(&agent_id), || async { true })
                .await
                .expect("initial detached delivery"),
            GuardedSend::Applied
        );
        let notices = Arc::new(Mutex::new(Vec::<DeliveryNotice>::new()));
        let recorded = notices.clone();
        registry.set_delivery_notice_sink(Arc::new(move |notice| {
            recorded.lock().unwrap().push(notice);
        }));

        let handle = registry
            .subscribe(&agent_id)
            .expect("attach byte-observation target");
        let mut user_writer = handle.writer.lock().await;
        tokio::time::pause();
        let (event_tx, event_rx) = broadcast::channel(8);
        let confirmation = tokio::spawn(confirm_prompt_delivery(
            registry.clone(),
            event_rx,
            ConfirmationTask {
                pane_id: PANE_ID.into(),
                agent_id: agent_id.clone(),
                prompt: PROMPT.into(),
                delivery_id: "detached-backstop-report".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        ));

        // Let the confirmation task install its first 500 ms watch timer before
        // moving virtual time. After `advance`, the only await it can reach is
        // the writer we still own, so the caller-side clock precheck has
        // necessarily completed before this test records the user's input.
        tokio::task::yield_now().await;
        tokio::time::advance(unconfirmed_retry_delay(1) + Duration::from_millis(1)).await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert!(
            !confirmation.is_finished(),
            "precondition: after the deterministic backoff the confirmation task must be blocked on the held writer"
        );
        user_writer
            .write_all(USER_DRAFT.as_bytes())
            .expect("write draft after caller precheck");
        user_writer
            .flush()
            .expect("flush draft after caller precheck");
        registry.note_user_input(PANE_ID);
        drop(user_writer);
        tokio::time::resume();
        confirmation
            .await
            .expect("writer-held backstop must terminate confirmation");

        let notices = notices.lock().unwrap();
        let notice_details = notices
            .iter()
            .map(|notice| notice.detail)
            .collect::<Vec<_>>();
        assert_eq!(
            notices.len(),
            1,
            "a writer-held refusal caused by user input must be durable pane state, not only `target went stale` in a log; notices={notice_details:?}"
        );
        drop(notices);
        drop(event_tx);
        registry.shutdown_all();
    }

    /// Scenario: Abandon a spawn prompt against its exact pane owner, then replace that owner and exhaust the 256-watch cap for a new delivery. Abandonment must report state without pane bytes, a stale report must not mark the replacement, and the 257th delivery must visibly report that it is unwatched.
    #[serial_test::serial(prompt_confirmation_tasks)]
    #[tokio::test]
    async fn abandonment_reports_state_and_never_writes_into_the_pane() {
        cancel_all_prompt_confirmations();
        const PANE_ID: &str = "abandon-notice-pane";
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = spawn_shell_target(&registry, PANE_ID);
        let notices = Arc::new(Mutex::new(Vec::<DeliveryNotice>::new()));
        let recorded = notices.clone();
        registry.set_delivery_notice_sink(Arc::new(move |notice| {
            recorded.lock().unwrap().push(notice);
        }));

        abandon_spawn_prompt(&registry, PANE_ID, &agent_id, "delivery-abandoned", 3, None);
        {
            let notices = notices.lock().unwrap();
            assert_eq!(notices.len(), 1, "abandonment must report exactly once");
            assert_eq!(notices[0].pane_id, PANE_ID);
            assert_eq!(notices[0].agent_id, agent_id);
            assert_eq!(notices[0].delivery_id, "delivery-abandoned");
        }
        // Reviewer blocker 3: the pane's own byte stream stays untouched. The
        // previous round wrote the diagnostic into the agent's input buffer,
        // where LF may be read as Enter and a later submit can carry it along as
        // part of the user's turn.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let output = registry.snapshot(&agent_id).expect("pane snapshot");
        assert!(
            !String::from_utf8_lossy(&output).contains("prompt"),
            "no diagnostic bytes may reach the agent's input: {:?}",
            String::from_utf8_lossy(&output)
        );

        // The pane changes hands: the old delivery's report belongs to nobody.
        registry
            .close_agent(&agent_id)
            .expect("close notice target");
        let replacement = spawn_shell_target(&registry, PANE_ID);
        assert_ne!(replacement, agent_id);
        abandon_spawn_prompt(&registry, PANE_ID, &agent_id, "delivery-abandoned", 3, None);
        assert_eq!(
            notices.lock().unwrap().len(),
            1,
            "a report against a replaced agent must be suppressed, not re-addressed"
        );

        // Fill exactly the configured watch budget with live pending tasks.
        // The next distinct pane is the 257th delivery and must publish the
        // visible state report that the old silent-cap behavior omitted.
        {
            let mut tasks = CONFIRMATION_TASKS.lock().unwrap();
            for index in 0..MAX_CONFIRMATION_TASKS {
                let pending = tokio::spawn(std::future::pending::<()>());
                tasks.insert(format!("cap-fill-{index}"), pending.abort_handle());
            }
        }
        let (cap_tx, cap_rx) = broadcast::channel(1);
        spawn_confirmation_task(
            registry.clone(),
            cap_rx,
            ConfirmationTask {
                pane_id: PANE_ID.into(),
                agent_id: replacement.clone(),
                prompt: "CAP-EXHAUSTED-PROMPT".into(),
                delivery_id: "cap-exhausted-257".into(),
                generation: None,
                can_report_prompts: true,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        );
        {
            let notices = notices.lock().unwrap();
            assert_eq!(
                notices.len(),
                2,
                "the 257th delivery must be visibly reported, not silently unwatched"
            );
            assert_eq!(notices[1].delivery_id, "cap-exhausted-257");
            assert!(
                notices[1].detail.contains("maximum") && notices[1].detail.contains("NOT"),
                "the report must explain that confirmation and retry are unavailable: {:?}",
                notices[1]
            );
        }
        drop(cap_tx);
        cancel_all_prompt_confirmations();
        registry.shutdown_all();
    }

    /// Scenario: Consume most of an absolute delivery deadline in a simulated readiness wait, then leave the reporting confirmation unanswered. The watch must publish abandonment within only the remaining budget, rather than granting itself a fresh confirmation deadline.
    #[tokio::test]
    async fn readiness_wait_is_inside_the_absolute_confirmation_deadline() {
        const PANE_ID: &str = "absolute-deadline-pane";
        const PROMPT: &str = "ABSOLUTE-DEADLINE-PROMPT";

        let registry = Arc::new(AgentPtyRegistry::new());
        let agent_id = spawn_byte_target(&registry, PANE_ID);
        let notices = Arc::new(Mutex::new(Vec::<DeliveryNotice>::new()));
        let recorded = notices.clone();
        registry.set_delivery_notice_sink(Arc::new(move |notice| {
            recorded.lock().unwrap().push(notice);
        }));
        let (_event_tx, event_rx) = broadcast::channel(8);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(300);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let confirmation = tokio::spawn(confirm_prompt_delivery(
            registry.clone(),
            event_rx,
            ConfirmationTask {
                pane_id: PANE_ID.into(),
                agent_id: agent_id.clone(),
                prompt: PROMPT.into(),
                delivery_id: "absolute-deadline-test".into(),
                generation: None,
                can_report_prompts: true,
                deadline,
            },
        ));
        tokio::time::timeout(Duration::from_millis(250), confirmation)
            .await
            .expect("confirmation must use only the deadline budget left after readiness")
            .expect("absolute-deadline confirmation task must not panic");
        assert_eq!(
            notices.lock().unwrap().len(),
            1,
            "abandonment must be visible by one total deadline, including readiness; elapsed={:?}",
            started.elapsed()
        );

        registry.shutdown_all();
    }

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

    /// Issue #454 review, item 4: each role is registered in the daemon's
    /// `AppState` AS ITS SPAWN LANDS, not after the whole loop.
    ///
    /// The loop `?`s out of `spawn_one`, so a role that fails to spawn abandons
    /// the orchestration — and with the registration sitting after the loop, the
    /// roles that had already started were left running with no `pane_role_map`
    /// / `pane_orchestration_map` entry anywhere. A `dot-agent-deck delegate`
    /// from such a survivor is rejected with "delegate from unknown pane", which
    /// is the same inert-orchestration failure the post-loop registration was
    /// added to fix, reached by a different route.
    ///
    /// Role 1 is a command that cannot be exec'd, so the failure is the loop's
    /// real early return rather than a simulated one. Note what this test does
    /// NOT assert: role 0's child is still running afterwards and the caller
    /// gets no handle with which to close it. That is `spawn`'s pre-existing
    /// error semantics — orphan-on-partial-failure — and is tracked separately;
    /// registration ordering neither causes nor cures it.
    #[cfg(unix)]
    #[tokio::test]
    async fn each_orchestration_role_is_registered_as_its_spawn_lands() {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};

        struct SilentNotifier;
        impl Notifier for SilentNotifier {
            fn notify(&self, _event: NotifyEvent) {}
        }

        let dir = tempfile::tempdir().expect("tempdir for the orchestration cwd");
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: crate::state::SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));

        let role = |idx: usize, name: &str, command: &str| RoleSpawn {
            role_index: idx,
            role_name: name.to_string(),
            command: command.to_string(),
            is_start_role: idx == 0,
        };
        let req = SpawnRequest {
            task_name: "partial-454".to_string(),
            working_dir: dir.path().to_string_lossy().into_owned(),
            command: None,
            prompt: "unused — the spawn fails before delivery".to_string(),
            resolved_target: Some(SpawnTarget::Orchestration {
                name: "partial-454".to_string(),
                roles: vec![
                    role(0, "orchestrator", "/bin/sh"),
                    role(1, "worker", "/nonexistent/dot-agent-deck-454"),
                ],
                config: Box::new(OrchestrationConfig {
                    name: "partial-454".to_string(),
                    roles: vec![OrchestrationRoleConfig {
                        name: "orchestrator".to_string(),
                        command: "/bin/sh".to_string(),
                        start: true,
                        description: None,
                        prompt_template: None,
                        clear: true,
                    }],
                }),
            }),
            compose_orchestrator_context: false,
        };

        let result = spawn(req, &registry, &SilentNotifier, None, false, Some(&state)).await;
        assert!(
            result.is_err(),
            "precondition: role 1 must fail to spawn, aborting the orchestration"
        );

        let guard = state.read().await;
        let registered: Vec<&String> = guard.pane_role_map.keys().collect();
        assert_eq!(
            registered.len(),
            1,
            "the role that DID spawn must be registered even though a later one \
             failed; pane_role_map={:?}",
            guard.pane_role_map
        );
        let pane_id = registered[0];
        assert_eq!(
            guard.pane_role_map.get(pane_id).map(String::as_str),
            Some("orchestrator")
        );
        assert!(
            guard.orchestrator_pane_ids.contains(pane_id),
            "the surviving start role must still be registered as the orchestrator"
        );
        assert!(
            guard.pane_orchestration_map.contains_key(pane_id),
            "…and must carry the orchestration identity `handle_delegate` routes on"
        );
        drop(guard);

        registry.shutdown_all();
    }

    /// Issue #454 review, item 5: the "valid" in this function's contract
    /// includes the byte cap, and a schedule / dispatch task name has no length
    /// bound of its own.
    ///
    /// An over-long id is not cosmetic. `AgentPtyRegistry::spawn_agent` refuses
    /// to RETAIN one — it stores `pane_id_env = None` while launching the child
    /// with the full value — so the pane exists but the registry cannot name it:
    /// `ListAgents` has nothing to join the live session onto and `StopAgent`
    /// can never recover the id to clean up with. That is issue #454's own
    /// symptom (`STATUS=- TOOL=-`, reconnect restoring `Idle`) surviving #454's
    /// fix, for every task whose name is long enough, plus a leak per fire.
    #[test]
    fn next_pane_id_stays_valid_for_an_unbounded_task_name() {
        use crate::agent_pty::{PANE_ID_ENV_MAX_LEN, is_valid_pane_id_env};
        let long = "a-very-long-scheduled-task-name".repeat(20);
        let single = next_pane_id(&long, None);
        let role = next_pane_id(&long, Some(7));
        for id in [&single, &role] {
            assert!(
                is_valid_pane_id_env(id),
                "an unbounded task name must still yield a retainable pane id \
                 (len={}, cap={PANE_ID_ENV_MAX_LEN}): {id}",
                id.len()
            );
            assert!(id.starts_with(SCHEDULE_PANE_ID_PREFIX));
        }
        // The uniqueness-bearing tail is what must never be truncated: two
        // fires of the same long name differ only by the counter.
        assert!(
            role.ends_with("-r7"),
            "the role suffix must survive: {role}"
        );
        assert_ne!(
            next_pane_id(&long, None),
            single,
            "truncation must not collapse two fires of one long name onto one id"
        );
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
