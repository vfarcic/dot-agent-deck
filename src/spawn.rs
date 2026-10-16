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
//!    posts the prompt to the spawned agent's inbox, bound to its registry id
//!    (PRD #1666). Nothing waits for the agent to be ready: the message waits
//!    in the inbox until the agent's component takes it or the agent answers
//!    the doorbell.
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
use std::time::Duration;

use tokio::sync::broadcast;

use chrono::Utc;

use crate::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions, TabMembership, command_needs_shell_wrap,
};
use crate::event::{AgentEvent, AgentType, BroadcastMsg, DISPLAY_NAME_METADATA_KEY, EventType};
use crate::project_config::{
    ProjectConfig, default_orchestration, load_project_config, resolve_orchestration_name,
};
use crate::scheduler::{Notifier, NotifyEvent};

/// Prefix every scheduler-spawned pane's `DOT_AGENT_DECK_PANE_ID` carries
/// (PRD #127 N3). Lets the manager dialog's live-status check match
/// schedule-owned panes specifically rather than colliding with a manually
/// spawned agent that happens to share a display name.
pub const SCHEDULE_PANE_ID_PREFIX: &str = "sched-";

/// Monotonic counter making each spawned pane's `DOT_AGENT_DECK_PANE_ID`
/// unique within a daemon lifetime (the first prompt's inbox post is addressed by it).
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
    /// Prompt posted as the spawned agent's / orchestrator's first inbox message.
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
    /// request's prompt as a task) instead of delivering `prompt` verbatim, and
    /// declare whether a human is attending the pane it opens.
    ///
    /// `Some` only for PRD #220 `dispatch`. Without it a dispatched
    /// orchestration's orchestrator is never told that it IS one, so it works
    /// alone while every worker waits for a delegation that cannot arrive.
    ///
    /// Issue #703 folded the [`crate::orchestrator_context::Attendance`] into
    /// this field rather than adding a
    /// second one beside it: the two cannot be answered independently, because
    /// composing a context without saying who is watching the pane is exactly
    /// the gap that let a dispatched coordinator inherit an interactive
    /// template's "STOP and wait for approval" step. `None` keeps its own
    /// meaning — deliver the prompt verbatim, compose nothing — so a path that
    /// composes nothing is never asked a question it has no answer for.
    ///
    /// Deliberately NOT enabled for the scheduler (#127) or issue-dispatch (#120),
    /// even though both have the identical defect and the composition is now shared.
    /// Turning it on there changes what lands in a SHIPPED feature's pane: the
    /// orchestrator receives a one-line pointer instead of the prompt text, and
    /// three #120/#127 e2e tests assert that text arriving verbatim (a `cat`-based
    /// stub never reads the file). That is #222's job to do deliberately, with those
    /// tests updated as part of it — not a side effect of the dispatcher PR.
    ///
    /// **`Some` also makes the context a PRECONDITION of the spawn** (issue
    /// #1065): if it cannot be published, [`spawn`] returns
    /// [`SpawnError::OrchestratorContext`] and starts no role at all, rather than
    /// starting the team with the orchestrator holding the bare task. `None` is
    /// unaffected — a path that composes nothing has no precondition to fail.
    pub compose_orchestrator_context: Option<crate::orchestrator_context::Attendance>,
    /// PRD #1666 S5: the sender `reason` the first prompt is posted under,
    /// which names it to the agent (`dispatch:<unit>`, `seed`). `None` is a
    /// scheduled task: `schedule:<task_name>`.
    pub seed_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("failed to create working_dir {path:?}: {message}")]
    WorkingDir { path: String, message: String },
    #[error("failed to spawn agent: {0}")]
    Agent(String),
    /// Issue #1065: an orchestration was asked to compose a coordinator context
    /// (`SpawnRequest::compose_orchestrator_context`) and the publish failed, so
    /// **no role was started**.
    ///
    /// Carries the publish error itself rather than a flattened `String` so a
    /// caller can choose its rendering: this variant's `Display` is the
    /// daemon-local [`detail`](crate::orchestrator_context::ContextPublishError::detail)
    /// — which is what `dispatch` wants, since it is the sentence that names the
    /// remedy — while a caller answering a narrower audience has
    /// [`client_sentence`](crate::orchestrator_context::ContextPublishError::client_sentence)
    /// available on the same value.
    #[error("refused to start the orchestration: {0}")]
    OrchestratorContext(crate::orchestrator_context::ContextPublishError),
    /// PRD #1487 re-check, reviewer R3: a restart of the daemon holds its
    /// admission freeze, so the agent was refused for now — not a failure of
    /// this spawn. Nothing was left running (an orchestration rolled back what
    /// it had started), and no `SpawnFailed` was raised: an unattended caller
    /// defers instead — retrying if the restart is called off, and leaving the
    /// work to the next fire of the successor daemon if it goes ahead.
    #[error("failed to spawn agent: {}", crate::agent_pty::ADMISSION_FROZEN_REASON)]
    DaemonRestarting,
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
    /// The registry agent id occupying [`Self::delivery_pane_id`] — the identity
    /// the prompt delivery was already gated on, surfaced so a caller can bind to
    /// the agent rather than to a pane id that will be recycled.
    ///
    /// Carried as a field beside the pane id rather than re-derived from
    /// [`Self::agents`] because the two must be a CONSISTENT pair: both are read
    /// from the same `SpawnedAgent` here, at the moment the spawn returns, which
    /// is what the dispatch return edge's eviction gate depends on (PR #1081
    /// review, Greptile finding 1).
    pub delivery_agent_id: String,
    /// PRD #1589: the per-instance token every role of an orchestration spawn
    /// carries, minted with it. `None` for a single agent.
    pub orchestration_id: Option<String>,
    /// PRD #1589: the dispatched-unit record this spawn registered, when it was
    /// started through [`spawn_dispatched_unit`]. `None` otherwise.
    pub unit_id: Option<String>,
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
    /// Issue #308: what agent this role runs — its `agent = "…"` declaration
    /// when the config made one, else the type derived from `command`.
    ///
    /// Resolved here, at the point the config is flattened, rather than at the
    /// spawn seam, because the spawn seam sees only a command. Carrying it
    /// keeps a DISPATCHED orchestration (`dot-agent-deck dispatch`, the
    /// scheduler) launching each role exactly as the TUI's `Ctrl+N` path does
    /// — the parity `orchestration/dispatch/003` pins for `clear = true`
    /// respawns, which read the spawn-time identity this value becomes.
    pub agent_type: Option<AgentType>,
}

/// The branch decision: orchestration tab vs single-agent card. Pure data so it
/// is unit-testable independent of the PTY/registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnTarget {
    /// A single-agent card. `command` is the schedule's command; `None` =
    /// `$SHELL` (resolved by the spawn path, mirroring the new-deck dialog).
    ///
    /// `inherited` (issue #1602) is `Some` when `command` is the command of the
    /// pane that dispatched this unit, and says how that pane ran it. `None` —
    /// all the scheduler and issue-dispatch paths ever pass — keeps the
    /// single-agent policy: the type derived from the command, and `/bin/sh`
    /// for a command line.
    SingleAgent {
        command: Option<String>,
        inherited: Option<InheritedLaunch>,
    },
    /// An orchestration tab rooted at the target dir.
    ///
    /// `config` is the CHOSEN orchestration's own config, carried through so the
    /// spawn can compose the orchestrator's context (roles + delegation protocol)
    /// without re-finding it by name — re-resolution is what let the listing and
    /// the spawn disagree in the first place. See
    /// [`crate::orchestrator_context::prepare_orchestrator_context`].
    Orchestration {
        name: String,
        roles: Vec<RoleSpawn>,
        config: Box<crate::project_config::OrchestrationConfig>,
    },
}

/// Issue #1602: how the pane a single unit copies its command from ran that
/// command, so the unit runs it the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedLaunch {
    /// The dispatcher's agent, when its spawn or its hooks said. `None` derives
    /// it from the command.
    pub agent_type: Option<AgentType>,
    /// The `SHELL` wrapper-choice override the dispatcher's spawn carried.
    /// `None` runs a command line under the daemon's default shell, as the
    /// dispatcher's did, rather than under the `/bin/sh` a scheduled single
    /// agent is pinned to — `source env.sh && claude` that works in the
    /// dispatcher's bash must not fail in its unit's `sh`.
    pub shell: Option<String>,
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

/// The `shape` STRING vocabulary, shared by every door that can set one.
///
/// Issue #835 gives `[[scheduled_tasks]]` a `shape` field, so the same three
/// words now arrive from three places — a hand-edited `schedules.toml`, `schedule
/// add --shape`, and `schedule update --shape` — and are re-read by the daemon at
/// fire time. One parser for all four so a value the CLI accepts can never be a
/// value the loader rejects (or, worse, one the fire silently reinterprets).
///
/// Deliberately NOT a `serde` enum on the config field: the stored form has to
/// round-trip as a plain TOML scalar (`shape = "orchestration:review"`), and the
/// name half is free-form, so the parse is a string split rather than a variant
/// match. The field is validated at LOAD (see
/// [`crate::config::validate_task`]) so a typo is a load error naming the task,
/// not a surprise on the next cron tick.
impl SpawnShapeOverride {
    /// Accepted forms: `single`, `orchestration`, `orchestration:<name>`.
    ///
    /// `Err` carries a message that names the offending value AND lists the
    /// accepted vocabulary, because every caller of this surfaces it to a human
    /// (a CLI error, a load error, or a fire-failure notification).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        match trimmed {
            "single" => Ok(Self::SingleAgent),
            "orchestration" => Ok(Self::Orchestration(None)),
            other => match other.strip_prefix("orchestration:") {
                // A named orchestration. The name is NOT validated here — whether
                // the target dir defines it is a fire-time question against that
                // dir's config, which can change between authoring and the next
                // fire. Only the empty name is rejected, since
                // `orchestration:` reads as a typo for `orchestration` and would
                // otherwise search for an orchestration literally named "".
                Some(name) if !name.trim().is_empty() => {
                    Ok(Self::Orchestration(Some(name.trim().to_string())))
                }
                Some(_) => Err(format!(
                    "invalid shape {raw:?}: `orchestration:` needs a name after the colon (or use bare `orchestration` for the directory's default)"
                )),
                None => Err(format!(
                    "invalid shape {raw:?}: expected `single`, `orchestration`, or `orchestration:<name>`"
                )),
            },
        }
    }
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
            inherited: None,
        }),
        Some(SpawnShapeOverride::Orchestration(None)) => {
            // THE default, resolved through the one shared rule
            // ([`crate::project_config::default_orchestration`]) that
            // `decide_target` below also uses. Issue #704: this arm and that one
            // used to disagree — this one took the first ROLE-BEARING block, that
            // one took the first ENTRY and degraded to a single-agent card when
            // slot 0 was roleless. Same question, same repo, two answers.
            let chosen = config
                .and_then(|cfg| default_orchestration(cfg, dir))
                .ok_or_else(|| {
                    format!(
                        "no orchestration with roles is defined in {}: add an \
                         `[[orchestrations]]` section with at least one role, or dispatch a \
                         single agent instead",
                        dir.display()
                    )
                })?;
            Ok(SpawnTarget::Orchestration {
                name: chosen.name.clone(),
                roles: roles_of(chosen.config),
                config: Box::new(chosen.config.clone()),
            })
        }
        Some(SpawnShapeOverride::Orchestration(Some(want))) => {
            let cfg = config.ok_or_else(|| {
                format!(
                    "no `.dot-agent-deck.toml` in {}, so no orchestration named '{want}' exists",
                    dir.display()
                )
            })?;
            // Roleless entries are skipped: two entries can resolve to the SAME
            // name (e.g. an unnamed `roles = []` plus a real one), and matching
            // the empty one would refuse a target the listing legitimately
            // offered. Issue #1396 item 4: two ROLE-BEARING entries under one
            // name are refused as ambiguous rather than resolved to the first —
            // the rule `PrepareOrchestration` follows since #1233, through the
            // same lookup, so a dispatch and a desktop launch of one config
            // cannot answer the same name differently.
            let orch = match crate::project_resolve::find_orchestration(cfg, want, dir) {
                Ok(orch) => orch,
                Err(crate::project_resolve::OrchestrationLookup::Ambiguous(count)) => {
                    return Err(format!(
                        "{}: {count} orchestrations with roles are named '{want}' in {}; \
                         rename one to dispatch it",
                        crate::daemon_protocol::PROJECT_ERR_AMBIGUOUS_ORCHESTRATION,
                        dir.display()
                    ));
                }
                Err(crate::project_resolve::OrchestrationLookup::Missing) => {
                    let available: Vec<String> = cfg
                        .orchestrations
                        .iter()
                        .filter(|o| !o.roles.is_empty())
                        .map(|o| resolve_orchestration_name(&o.name, dir))
                        .collect();
                    return Err(if available.is_empty() {
                        format!("no orchestration named '{want}', and none are defined")
                    } else {
                        format!(
                            "no orchestration named '{want}'; available: {}",
                            available.join(", ")
                        )
                    });
                }
            };
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
///
/// `is_start_role` is the orchestrator SEAT, not the bare `start` flag: it is
/// computed here, once, by [`OrchestrationConfig::orchestrator_role_index`]
/// (issue #523), so the membership stamped on each pane, the surface the TUI
/// builds the tab from and the `orchestrator_pane_ids` registration all name
/// the same one role — the role a `Ctrl+n` tab of the same config seats.
///
/// [`OrchestrationConfig::orchestrator_role_index`]: crate::project_config::OrchestrationConfig::orchestrator_role_index
fn roles_of(orch: &crate::project_config::OrchestrationConfig) -> Vec<RoleSpawn> {
    let orch_idx = orch.orchestrator_role_index();
    orch.roles
        .iter()
        .enumerate()
        .map(|(i, r)| RoleSpawn {
            role_index: i,
            role_name: r.name.clone(),
            command: r.command.clone(),
            is_start_role: i == orch_idx,
            agent_type: r.resolved_agent_type(),
        })
        .collect()
}

/// Decide what to open from the target dir's config and the schedule's command.
/// A spawnable `[[orchestrations]]` → orchestration; otherwise a single-agent
/// card. `dir` is used only to resolve an unnamed orchestration's name to its
/// cwd-basename (matching the TUI/daemon contract).
///
/// WHICH orchestration is [`default_orchestration`]'s answer, shared with
/// [`decide_target_with_override`]'s bare form. Issue #704: this function used to
/// inspect `orchestrations.first()` alone, so a roleless placeholder in slot 0
/// sent a scheduled fire to a SINGLE-AGENT CARD in a repo whose second block was
/// perfectly spawnable — and whose `--list-targets` listing was offering it. The
/// bare dispatch form had already been fixed; the scheduler had not, and nothing
/// held the two together. Now one function answers for both.
pub fn decide_target(
    config: Option<&ProjectConfig>,
    dir: &Path,
    schedule_command: Option<&str>,
) -> SpawnTarget {
    if let Some(chosen) = config.and_then(|cfg| default_orchestration(cfg, dir)) {
        return SpawnTarget::Orchestration {
            name: chosen.name.clone(),
            roles: roles_of(chosen.config),
            config: Box::new(chosen.config.clone()),
        };
    }
    SpawnTarget::SingleAgent {
        command: schedule_command.map(|c| c.to_string()),
        inherited: None,
    }
}

/// Index (into `roles`) of the orchestrator — the role the prompt is delivered
/// to and the one registered as able to `delegate`. The one rule every path
/// reads, [`crate::project_config::orchestrator_index`] (issue #523): the first
/// `start = true` role, else the role named `orchestrator`, else the first.
/// `roles` is assumed non-empty (callers only build an `Orchestration` target
/// with ≥1 role).
///
/// Before #523 this tried the NAME first, so a flagged role beside a
/// differently positioned role named `orchestrator` was a worker here and the
/// orchestrator on the `Ctrl+n` tab. [`roles_of`] already flags the seat, so
/// for a target it built this finds that role; the name fallback still covers a
/// hand-built `RoleSpawn` list.
pub fn orchestrator_role_index(roles: &[RoleSpawn]) -> usize {
    crate::project_config::orchestrator_index(roles, |r| r.role_name.as_str(), |r| r.is_start_role)
}

/// Open a tab for `req` and deliver its prompt. See the module docs for the
/// full contract. On `working_dir` creation or spawn failure, surfaces the
/// reason via `notifier` and returns `Err` without panicking.
///
/// `detach_delivery` no longer changes anything (PRD #1666 S5): every pane is
/// spawned and registered, and its prompt posted to the pane's inbox
/// ([`post_seed`]), before `spawn` returns, and nothing waits for the agent. It
/// is kept so callers need not change. A refused post is logged and still
/// returns `Ok`, because the pane exists either way.
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
    spawn_with_unit(
        req,
        registry,
        notifier,
        event_tx,
        detach_delivery,
        state,
        None,
    )
    .await
}

/// PRD #1589 D5: [`spawn`] for a dispatched unit — the same spawn, which also
/// records the unit in the registry's
/// [`crate::dispatched_units::DispatchedUnits`], and retains `return_to` as the
/// recipient of its completion report (PRD #220's return edge), once every pane
/// it opens exists and BEFORE the task is delivered.
///
/// Both used to be impossible to register in time: the return route was
/// registered by `handle_dispatch` after `spawn` returned, and `spawn` returns
/// only after the delivery wait, so a unit that reported `work-done --done`
/// the moment it started found no route and its report was dropped as "from
/// unknown pane" (auditor S3, reproduced by `dispatch/close-verb`'s stand-in
/// units). A spawn that fails registers neither, because the registration
/// comes after the last step that can fail.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_dispatched_unit(
    req: SpawnRequest,
    registry: &Arc<AgentPtyRegistry>,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    detach_delivery: bool,
    state: Option<&crate::state::SharedState>,
    unit: crate::dispatched_units::UnitOrigin,
    return_to: crate::dispatch_return::DispatchCaller,
) -> Result<SpawnHandle, SpawnError> {
    spawn_with_unit(
        req,
        registry,
        notifier,
        event_tx,
        detach_delivery,
        state,
        Some((unit, return_to)),
    )
    .await
}

/// [`spawn_dispatched_unit`]'s registration, at the one point both spawn
/// shapes call it: the unit's identities exist, its task is not yet delivered.
fn register_dispatched_unit(
    registry: &AgentPtyRegistry,
    unit: Option<(
        crate::dispatched_units::UnitOrigin,
        crate::dispatch_return::DispatchCaller,
    )>,
    kind: crate::dispatched_units::UnitKind,
) -> Option<String> {
    let (origin, caller) = unit?;
    let (unit_pane_id, unit_agent_id) = {
        let (pane, agent) = kind.terminal();
        (pane.to_string(), agent.to_string())
    };
    let id = registry
        .dispatched_units()
        .register(origin.into_unit(kind, chrono::Utc::now().timestamp_millis()));
    tracing::debug!(
        unit_id = %id,
        unit_pane_id = %unit_pane_id,
        unit_agent_id = %unit_agent_id,
        caller_pane_id = %caller.pane_id,
        // PRD #220 Phase 2 review (finding A4), missed here and caught by PR
        // #1081's review (Greptile finding 3): the unit name is producer-supplied
        // and rode into this field raw, where a bare LF forges a log line, a CR
        // overwrites the one being written and a bidi override reorders whatever
        // renders it.
        unit = %crate::config_validation::escape_field_for_log(
            &caller.unit_name,
            crate::config_validation::MAX_QUOTED_VALUE_CHARS,
        ),
        "dispatch: recorded the unit and retained the caller for its completion report"
    );
    // PRD #220 M2.0: retain the caller against the unit's TERMINAL pane — the
    // single agent's pane, or the orchestration's start role — which is the pane
    // a `work-done --done` will arrive under. Bound to the unit's AGENT as well
    // as its pane: a pane id is a recycled handle, and a predecessor's late EOF
    // would otherwise evict this route out from under the agent that now holds
    // the pane (PR #1081 review, Greptile finding 1).
    registry.register_dispatch_return(&unit_pane_id, &unit_agent_id, caller);
    Some(id)
}

async fn spawn_with_unit(
    req: SpawnRequest,
    registry: &Arc<AgentPtyRegistry>,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    detach_delivery: bool,
    state: Option<&crate::state::SharedState>,
    unit: Option<(
        crate::dispatched_units::UnitOrigin,
        crate::dispatch_return::DispatchCaller,
    )>,
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
        None => {
            let cfg = load_config_for_dir(dir);
            // Issue #704: when the config left the choice to file order, say so.
            // This path has no user in front of it — a cron tick or an
            // issue-dispatch fire — so the daemon log is the only place the
            // record can land. `dispatch` (which does have a caller) puts the
            // same sentence in its reply, and `--list-targets` / `validate` show
            // it to the config's author before either ever fires.
            if let Some(note) = cfg
                .as_ref()
                .and_then(|c| default_orchestration(c, dir))
                .and_then(|chosen| chosen.diagnostic())
            {
                tracing::warn!(
                    // `task_name` is `dispatch-<name>` on the dispatch path, so it
                    // carries a producer-supplied name into a tracing field — the
                    // class PR #1081 review (Greptile finding 3) swept for.
                    task = %crate::config_validation::escape_field_for_log(
                        &req.task_name,
                        crate::config_validation::MAX_QUOTED_VALUE_CHARS,
                    ),
                    dir = %dir.display(),
                    "{note}"
                );
            }
            decide_target(cfg.as_ref(), dir, req.command.as_deref())
        }
    };

    // 3. Spawn + deliver.
    match target {
        SpawnTarget::SingleAgent { command, inherited } => {
            // PR #1735 review: a first prompt the inbox would refuse fails the
            // spawn before anything is started.
            let reason = seed_reason(&req);
            if let Err(error) = admit_seed(registry, &reason, &req.prompt) {
                return Err(seed_refused(notifier, &req.task_name, &error));
            }
            let pane_id = next_pane_id(&req.task_name, None);
            // PRD #127 C2: only pin the `-c` wrapper shell to a deterministic
            // `/bin/sh` when the command ACTUALLY needs shell-wrapping (it has
            // whitespace → a shell command line). A single bare word is exec'd
            // directly (no shell), and an omitted command falls back to the
            // daemon's `$SHELL` (mirrors the new-deck dialog) — in neither case
            // do we pin (or leak) a SHELL override.
            //
            // Issue #1602: a command inherited from a dispatcher runs under the
            // shell the dispatcher's ran under instead.
            let (agent_type, shell) = match inherited {
                Some(InheritedLaunch { agent_type, shell }) => (agent_type, shell),
                None => (
                    None,
                    command
                        .as_deref()
                        .is_some_and(command_needs_shell_wrap)
                        .then(|| crate::platform::shell::fixed_command_shell("/bin/sh")),
                ),
            };
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
                // …and no role config either. Only a dispatched unit carries a
                // type (its dispatcher's, issue #1602); otherwise it is derived
                // from the command exactly as before (issue #308).
                agent_type.clone(),
                shell,
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
            //
            // PRD #1589 (auditor S1): the card and the unit record are
            // published under the state WRITE guard, and only while the agent's
            // record is still in the registry. A close removes the record
            // before it takes that guard to unregister the pane and announce
            // the removal, so a close that won the race leaves nothing to
            // publish — the spawn is answered as closed — and one that lost it
            // announces its removal after this card and ends this record.
            registry.pause_point("dispatch-publication").await;
            let publish_guard = match state {
                Some(state) => Some(state.write().await),
                None => None,
            };
            if !registry.generation_registered(&id) {
                drop(publish_guard);
                tracing::info!(
                    agent_id = %id,
                    "spawn: the agent was closed before it was published; publishing nothing \
                     for it"
                );
                return Err(SpawnError::Agent(CLOSED_WHILE_STARTING.to_string()));
            }
            if let Some(tx) = event_tx {
                surface_spawned_pane(
                    tx,
                    &pane_id,
                    &req.working_dir,
                    command.as_deref(),
                    // The same identity the registry was given above, so the
                    // live card is badged as that agent from its first frame.
                    agent_type,
                    Some(&req.task_name),
                    &id,
                );
            }
            // PRD #1589 D5: the unit's record, before its task is delivered.
            let unit_id = register_dispatched_unit(
                registry,
                unit,
                crate::dispatched_units::UnitKind::Single {
                    pane_id: pane_id.clone(),
                    agent_id: id.clone(),
                },
            );
            drop(publish_guard);
            // PRD #1666 S5: the prompt is the pane's first inbox message, posted
            // now and bound to the agent just spawned. Nothing waits for it to
            // start a session; nothing is typed.
            let _ = detach_delivery;
            if let Err(error) = post_seed(registry, &pane_id, &id, &reason, &req.prompt) {
                let started = [SpawnedAgent {
                    id: id.clone(),
                    pane_id: pane_id.clone(),
                    role_name: None,
                }];
                close_started_agents(registry, state, event_tx, &started).await;
                return Err(seed_refused(notifier, &req.task_name, &error));
            }
            Ok(SpawnHandle {
                task_name: req.task_name,
                kind: SpawnKind::SingleAgent,
                agents: vec![SpawnedAgent {
                    id: id.clone(),
                    pane_id: pane_id.clone(),
                    role_name: None,
                }],
                delivery_pane_id: pane_id,
                delivery_agent_id: id,
                orchestration_id: None,
                unit_id,
                on_tab_closed: None,
            })
        }
        SpawnTarget::Orchestration {
            name,
            roles,
            config: orch_config,
        } => {
            let orch_idx = orchestrator_role_index(&roles);
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
            // not submit reliably through a PTY and task text is arbitrary.
            //
            // **Issue #1065: a failed publish REFUSES the spawn.** This used to be
            // `prepare_orchestrator_prompt(…).unwrap_or_else(|| req.prompt.clone())`
            // — one `warn!` in the daemon log, and then the team started with the
            // orchestrator holding the bare task text and no role template, no
            // `## Available agents`, no `## Delegation protocol`. It also LOOKS
            // healthy: the pane labels, the role cards and the `daemon status`
            // rows are indistinguishable from a working team, so the first role
            // implements the whole task solo while the rest idle. The
            // fallback is defensible where it came from — for a single agent the
            // prompt IS the whole story — but for an orchestration the wrapper is
            // the instruction, so delivering the task without it produces an agent
            // doing the opposite of what the shape was chosen for. Measured twice
            // on PRD #742.
            //
            // Composed HERE, before the role loop, rather than where the old
            // fallback sat (after it, just above the delivery): that is what
            // keeps the refusal from spending an agent. No role has been spawned
            // yet, so there is nothing for `roll_back_partial_orchestration` to
            // tear down, and `dispatch`'s rollback reclaims the worktree and branch
            // against a tree nothing ever occupied. It also fits the invariant
            // issue #600 already established for this branch — `Err` from `spawn`
            // means "nothing is running" — instead of inventing a second,
            // half-started outcome beside it.
            // Issue #1395: the published file is kept beside the prompt so the
            // orchestrator's role registration below can record it.
            let (prompt, context_path) = match req.compose_orchestrator_context {
                Some(attendance) => {
                    let prepared = crate::orchestrator_context::prepare_orchestrator_context(
                        &orch_config,
                        Path::new(&req.working_dir),
                        Some(req.prompt.as_str()),
                        attendance,
                    )
                    .map_err(|e| {
                        // `error!`, not `warn!`: the old `warn!` inside
                        // `prepare_orchestrator_prompt` was the only thing the
                        // daemon log said about a degradation that cost a whole
                        // PRD's worth of agent time — one line, in a log nobody
                        // reads during a fire-and-forget dispatch, while the reply
                        // written into the caller's pane said the spawn worked.
                        tracing::error!(
                            orchestration = %name,
                            dir = %req.working_dir,
                            reason = %e,
                            "spawn: refusing to start the orchestration — the coordinator \
                             context could not be published"
                        );
                        // The same notification a failed `spawn_agent` raises, so
                        // every door that already reports an abandoned spawn
                        // reports this one too rather than needing to learn a new
                        // variant.
                        notifier.notify(NotifyEvent::SpawnFailed {
                            task: req.task_name.clone(),
                            message: format!(
                                "refused to start orchestration '{name}': {}",
                                e.detail()
                            ),
                        });
                        SpawnError::OrchestratorContext(e)
                    })?;
                    (prepared.prompt, Some(prepared.context_path))
                }
                // #120 / #127: unchanged — the prompt is delivered verbatim, and
                // nothing is composed, so there is nothing to refuse. See
                // `compose_orchestrator_context` for why this is not flipped here.
                None => (req.prompt.clone(), None),
            };
            // PR #1735 review: the composed prompt is what the orchestrator's
            // inbox is posted, so it is what is admitted, before any role starts.
            let reason = seed_reason(&req);
            if let Err(error) = admit_seed(registry, &reason, &prompt) {
                return Err(seed_refused(notifier, &req.task_name, &error));
            }
            let mut agents = Vec::with_capacity(roles.len());
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
            let identity = crate::state::OrchestrationIdentity {
                id: orchestration_id.clone(),
                name: name.clone(),
            };
            // Issue #960: minted once, before the loop, for the same reason
            // `orchestration_id` is — every role of one orchestration shares it
            // — and stamped on every role's membership below so it SURVIVES a
            // detach/reattach. It used to be computed inside
            // `surface_spawned_orchestration` and put on the transient broadcast
            // alone, while this loop hardcoded `display_title: None`. Nothing
            // persists a broadcast, so on reattach `partition_hydrated_panes`
            // read `None` off every role pane and
            // `open_orchestration_tab_with_existing_role_panes` fell back to the
            // canonical config name: a tab labelled `mixed · issue-950` while
            // attached came back as bare `mixed`, and N concurrent dispatches all
            // came back identically labelled — the exact confusion the suffix
            // exists to prevent. The broadcast now carries THIS value (see the
            // `surface_spawned_orchestration` call below), so the live label and
            // the reattached label cannot disagree by construction.
            //
            // Issue #1339: and ADMITTED here, before the first role, through the
            // same uniqueness check a client `StartAgent` passes (#555). The
            // derived title names the orchestration and the directory, so two
            // live runs of one orchestration in one directory — two schedules
            // firing one repo, or a fresh-tab schedule firing again before its
            // last run ended — derived byte-identical labels. A taken title is
            // suffixed (`… · 2`) rather than refused; see
            // `AppState::claim_dispatched_orchestration_title` for why. The
            // claim is held until every role is registered (each registered
            // pane then holds the title) and released on the rollback arm too.
            let derived_title = dispatched_orchestration_display_title(&name, &req.working_dir);
            // With no daemon state (tests) there is nothing to claim against,
            // and every release below is gated on the same `state`.
            let display_title = match state {
                Some(state) => {
                    let title_cwd =
                        crate::state::orchestration_title_cwd_key(&req.working_dir).await;
                    state.write().await.claim_dispatched_orchestration_title(
                        &identity,
                        derived_title.as_deref(),
                        &title_cwd,
                        registry,
                    )
                }
                None => derived_title,
            };
            for (idx, role) in roles.iter().enumerate() {
                let pane_id = next_pane_id(&req.task_name, Some(role.role_index));
                let membership = TabMembership::Orchestration {
                    name: name.clone(),
                    role_index: role.role_index,
                    role_name: role.role_name.clone(),
                    is_start_role: role.is_start_role,
                    orchestration_cwd: Some(req.working_dir.clone()),
                    display_title: display_title.clone(),
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
                    // Issue #308: the role's resolved type, so a dispatched
                    // orchestration wraps and badges a declared launcher role
                    // identically to the TUI path.
                    role.agent_type.clone(),
                    None,
                    notifier,
                );
                // Issue #600: an orchestration spawn is ALL-OR-NOTHING. This used
                // to be a bare `?`, which returned the error while every role
                // already launched stayed live in the registry — with no
                // `SpawnHandle` returned, the caller had nothing to close them
                // with and no tab was ever surfaced, so those children were
                // unreachable by any close path the user has. Tear them down here
                // instead, so `Err` from this function means "nothing is running".
                //
                // Chosen over returning a PARTIAL handle because a half-spawned
                // orchestration is not a usable product: the orchestrator has been
                // handed a context file naming roles that do not exist, its
                // delegations to them cannot route, and the caller
                // (`handle_dispatch`, `issue_dispatch_run`) has no way to surface
                // a tab for it — the `OrchestrationSurface` broadcast below is
                // never reached. See the issue and the PR for the full argument.
                let id = match id {
                    Ok(id) => id,
                    Err(e) => {
                        roll_back_partial_orchestration(
                            registry,
                            state,
                            event_tx,
                            &agents,
                            &name,
                            &role.role_name,
                        )
                        .await;
                        // Issue #1339: nothing is left running, so nothing
                        // holds the title this spawn was admitted under.
                        if let Some(state) = state {
                            state
                                .write()
                                .await
                                .release_orchestration_title_claim(&identity);
                        }
                        return Err(e);
                    }
                };
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
                // (Issue #600 has since decided the other half: the loop no
                // longer just `?`s out — it rolls the already-spawned roles back,
                // so `Err` from this function means nothing is left running. This
                // registration is what the rollback UNDOES, and it still has to
                // happen here rather than after the loop: the rollback tears down
                // a role only after that role's `close_agent` has removed its
                // child, so between the two a live child must never be missing its
                // routing entry.)
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
                // registry stores `pane_id_env = None` for it, so a
                // pane-keyed write could never route to the pane the
                // role map claimed to have.
                // PRD #1589 (auditor S1): not for a role a close already took
                // down since it was published — re-checked under the write
                // guard below, which the close's unregister needs too. A role
                // found closed aborts the whole construction after the loop
                // body, below.
                registry
                    .pause_point(&format!("dispatch-role:{}", role.role_name))
                    .await;
                let mut closed_while_starting = false;
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
                    let title_cwd =
                        crate::state::orchestration_title_cwd_key(&req.working_dir).await;
                    let mut state = state.write().await;
                    if !registry.generation_registered(&id) {
                        tracing::info!(
                            agent_id = %id,
                            role = %role.role_name,
                            "spawn: the role was closed before it was registered; \
                             registering nothing for it"
                        );
                        closed_while_starting = true;
                    } else {
                        // Issue #962: the daemon holds the run title itself, beside
                        // the role maps, so a `clear = true` worker re-created later
                        // does not have to find a live sibling to read it from.
                        // Already admitted before the loop (issue #1339), so this
                        // keeps the claimed title rather than checking it again.
                        state.record_orchestration_title(
                            &identity,
                            display_title.as_deref(),
                            &title_cwd,
                        );
                        state.register_orchestration_role(
                            &pane_id,
                            &role.role_name,
                            // `orch_idx`: this path's authority on which role is
                            // the orchestrator — the pane that receives the
                            // orchestrator context and the caller's task below. It
                            // is the same rule (`project_config::orchestrator_index`)
                            // the `Ctrl+n` tab sends in its membership, so both
                            // `AttachRequest::StartAgent` and this path register the
                            // same one pane for one config (issue #523).
                            idx == orch_idx,
                            identity.clone(),
                            Some(req.working_dir.as_str()),
                        );
                        if let Some(tx) = event_tx {
                            state.announce_unproven_evictions(tx);
                        }
                        // Issue #1395: the orchestrator's own context file, for its
                        // `ListAgents` record and for removal when this ends.
                        if idx == orch_idx
                            && let Some(path) = context_path.clone()
                        {
                            state.record_orchestration_context(&identity, path);
                        }
                    }
                }
                agents.push(SpawnedAgent {
                    id,
                    pane_id,
                    role_name: Some(role.role_name.clone()),
                });
                if closed_while_starting {
                    return Err(abort_closed_orchestration(
                        registry, state, event_tx, &agents, &identity,
                    )
                    .await);
                }
            }
            // PRD #1589 (auditor S1): what follows publishes the orchestration
            // — its tab, its cards and its unit record — under the state WRITE
            // guard, after checking that every role is still registered and no
            // close finished the instance. A close removes a role's record
            // before it takes that guard to unregister the pane and announce the
            // removal, so either this sees a closed role and aborts, publishing
            // nothing, or the close's removal follows this publication.
            registry.pause_point("dispatch-publication").await;
            let mut publish_guard = match state {
                Some(state) => Some(state.write().await),
                None => None,
            };
            if agents
                .iter()
                .any(|agent| !registry.generation_registered(&agent.id))
                || registry.is_instance_ended(&orchestration_id)
            {
                drop(publish_guard);
                return Err(abort_closed_orchestration(
                    registry, state, event_tx, &agents, &identity,
                )
                .await);
            }
            // Issue #1339: every role is registered, so its panes hold the
            // title from here on and the spawn's own claim ends.
            if let Some(guard) = publish_guard.as_mut() {
                guard.release_orchestration_title_claim(&identity);
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
                surface_spawned_orchestration(
                    tx,
                    &name,
                    &req.working_dir,
                    // Issue #960: the SAME value the role memberships above were
                    // stamped with, not a second computation of it.
                    display_title.clone(),
                    &roles,
                    &agents,
                    &orchestration_id,
                    // Issue #1395: the file this spawn published for its
                    // orchestrator — the same value recorded above — so a live
                    // tab re-arms from it rather than from the mirror.
                    context_path.as_deref(),
                );
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
                // names. These events carry `agent_id: None`. When the role's real
                // `SessionStart` arrives it supersedes this placeholder and
                // INHERITS the name (PRD #127 finding #2), so the label survives
                // the handover instead of reverting to a UUID.
                for (role, agent) in roles.iter().zip(agents.iter()) {
                    surface_spawned_pane(
                        tx,
                        &agent.pane_id,
                        &req.working_dir,
                        Some(&role.command),
                        role.agent_type.clone(),
                        Some(&role.role_name),
                        &agent.id,
                    );
                }
            }
            // The prompt goes to the orchestrator role pane's inbox, bound to
            // that pane's registry agent_id.
            let delivery_pane_id = agents[orch_idx].pane_id.clone();
            let delivery_agent_id = agents[orch_idx].id.clone();
            // PRD #1589 D5: the unit's record, before the orchestrator's task is
            // delivered. Its terminal generation is the orchestrator the task
            // goes to, which is the only generation whose `--done` completes it.
            // Still under the publication guard (auditor S1), so a close of the
            // instance either ended it before this check or ends this record
            // when it stops the last role.
            let unit_id = register_dispatched_unit(
                registry,
                unit,
                crate::dispatched_units::UnitKind::Orchestration {
                    orchestration_id: orchestration_id.clone(),
                    name: name.clone(),
                    terminal_pane_id: delivery_pane_id.clone(),
                    terminal_agent_id: delivery_agent_id.clone(),
                },
            );
            drop(publish_guard);
            // PRD #1666 S5: posted to the orchestrator role's inbox, as above.
            if let Err(error) = post_seed(
                registry,
                &delivery_pane_id,
                &delivery_agent_id,
                &reason,
                &prompt,
            ) {
                close_started_agents(registry, state, event_tx, &agents).await;
                if let Some(state) = state {
                    state
                        .write()
                        .await
                        .release_orchestration_title_claim(&identity);
                }
                return Err(seed_refused(notifier, &req.task_name, &error));
            }
            Ok(SpawnHandle {
                task_name: req.task_name,
                kind: SpawnKind::Orchestration { name },
                agents,
                delivery_pane_id,
                delivery_agent_id,
                orchestration_id: Some(orchestration_id),
                unit_id,
                on_tab_closed: None,
            })
        }
    }
}

/// PRD #1589 (auditor S1): why a spawn that a close overtook answers `Err`.
pub const CLOSED_WHILE_STARTING: &str =
    "it was closed while it was starting; nothing was published or recorded for it";

/// PRD #1589 (auditor S1): end an orchestration construction that found one of
/// its roles closed. A close of the instance stops every role it enumerated and
/// refuses any later one, so it owns the teardown and this stops nothing; a
/// role stopped some other way (a `StopAgent` of one pane) leaves the rest to
/// this, which rolls back the roles still registered, as a failed role does.
/// The title claim is released either way, and the returned error is what the
/// spawn answers: no tab, no card and no unit record is published.
async fn abort_closed_orchestration(
    registry: &Arc<AgentPtyRegistry>,
    state: Option<&crate::state::SharedState>,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    started: &[SpawnedAgent],
    identity: &crate::state::OrchestrationIdentity,
) -> SpawnError {
    tracing::warn!(
        orchestration = %identity.name,
        orchestration_id = %identity.id,
        "spawn: a role of this orchestration was closed while it was starting; publishing \
         nothing for it"
    );
    if !registry.is_instance_closing(&identity.id) {
        let still: Vec<SpawnedAgent> = started
            .iter()
            .filter(|agent| registry.generation_registered(&agent.id))
            .cloned()
            .collect();
        roll_back_partial_orchestration(registry, state, event_tx, &still, &identity.name, "-")
            .await;
    }
    if let Some(state) = state {
        state
            .write()
            .await
            .release_orchestration_title_claim(identity);
    }
    SpawnError::Agent(CLOSED_WHILE_STARTING.to_string())
}

/// Issue #600: tear down the roles an orchestration spawn had already started
/// when a LATER role failed, so [`spawn`]'s `Err` means "nothing is running".
///
/// Before this, the role loop `?`d straight out: roles `0..N` stayed live in
/// `AgentPtyRegistry` as PTY children, no `SpawnHandle` came back so the caller
/// had nothing to close them with, and the `OrchestrationSurface` broadcast that
/// would have given the user a tab is never reached on the failure path — so the
/// children were unreachable by every close path that exists. It also left
/// `handle_dispatch`'s rollback force-removing the worktree those children were
/// rooted in (issue #575); with the teardown in place that rollback runs against
/// a tree nothing occupies, which is the state its comment always claimed.
///
/// Best-effort by construction, and it never reports failure upwards: the caller
/// is already returning the ORIGINAL spawn error, which is what the user needs to
/// see. A role that somehow survives is caught downstream instead — the dispatch
/// rollback re-asks `worktree_still_in_use` before it removes anything.
///
/// Uses the same close protocol as the `AttachRequest::StopAgent` handler
/// (`begin_pane_close` → `close_agent` → `unregister_pane` → `finish_pane_close`)
/// rather than a bare `close_agent`, because the roles that DID start are already
/// registered in `pane_role_map` and a fast orchestrator could in principle have
/// armed a delegation against one of them before the failing role was reached.
async fn roll_back_partial_orchestration(
    registry: &Arc<AgentPtyRegistry>,
    state: Option<&crate::state::SharedState>,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    started: &[SpawnedAgent],
    orchestration: &str,
    failed_role: &str,
) {
    if started.is_empty() {
        return;
    }
    tracing::warn!(
        orchestration = %orchestration,
        failed_role = %failed_role,
        rolling_back = started.len(),
        "spawn: a later orchestration role failed; tearing down the roles that already started"
    );
    close_started_agents(registry, state, event_tx, started).await;
}

/// The teardown [`roll_back_partial_orchestration`] runs, for any agents a
/// spawn started and must not leave running: the `StopAgent` close protocol
/// for each of `started`. Best-effort, as that function describes.
///
/// PR #1735 third review: an agent this rolls back may already have been seen
/// — the spawn surfaced its card, or the agent's own `SessionStart` hook did —
/// so, as `StopAgent` does, each pane is closed under a cleanup hold
/// ([`AgentPtyRegistry::hold_pane_for_cleanup`]): its pane-scoped cleanup runs
/// only while no other generation claims the pane, and then the pane's
/// sessions go and its closure is announced to every attached client
/// (`event_tx`). The unit the agent belonged to ends as a stop ends it.
pub(crate) async fn close_started_agents(
    registry: &Arc<AgentPtyRegistry>,
    state: Option<&crate::state::SharedState>,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
    started: &[SpawnedAgent],
) {
    if started.is_empty() {
        return;
    }

    let records: Vec<Option<crate::agent_pty::AgentRecord>> = started
        .iter()
        .map(|agent| registry.agent_record_any(&agent.id))
        .collect();
    let holds: Vec<Option<crate::agent_pty::PaneCleanupHold>> = started
        .iter()
        .map(|agent| registry.hold_pane_for_cleanup(&agent.pane_id, &agent.id))
        .collect();
    let held_panes: Vec<&str> = holds
        .iter()
        .flatten()
        .map(crate::agent_pty::PaneCleanupHold::pane_id)
        .collect();

    // Refuse new delegations to every one of these panes BEFORE the first child
    // is terminated, so the teardown cannot race one in against a pane whose
    // agent is mid-SIGTERM.
    for pane in &held_panes {
        registry.begin_pane_close(pane);
    }

    // `close_agent` runs the synchronous SIGTERM-with-grace loop
    // (`terminate_child_with_grace_and_wait`, up to `AGENT_TERMINATE_GRACE` per
    // child), so it must not run on a Tokio worker thread — same reasoning as the
    // `StopAgent` handler's `spawn_blocking` hop.
    let registry_for_close = registry.clone();
    let ids: Vec<String> = started.iter().map(|a| a.id.clone()).collect();
    let closed = tokio::task::spawn_blocking(move || {
        ids.into_iter()
            .map(|id| {
                let outcome = registry_for_close.close_agent(&id);
                (id, outcome)
            })
            .collect::<Vec<_>>()
    })
    .await;
    match closed {
        Ok(outcomes) => {
            for (id, outcome) in outcomes {
                if let Err(e) = outcome {
                    // `NotFound` is ordinary here — the child may have exited on
                    // its own between spawning and the failure of a later role.
                    tracing::warn!(
                        agent_id = %id,
                        error = %e,
                        "spawn rollback: could not close an already-started role"
                    );
                }
            }
        }
        Err(join_err) => {
            tracing::warn!(
                error = %join_err,
                "spawn rollback: the close task panicked or was cancelled; \
                 already-started agents may still be running"
            );
        }
    }

    // Drop the routing identity these panes were given as each spawn landed
    // (`register_orchestration_role`, above), and any session the agents' hooks
    // created on them. Done AFTER the children are gone so a live child is never
    // missing its `pane_role_map` entry — the inverse ordering is exactly the
    // inconsistency issue #454 closed.
    if let Some(state) = state {
        let mut guard = state.write().await;
        for pane in &held_panes {
            guard.unregister_pane(pane);
            guard.remove_sessions_for_pane(pane);
        }
    }
    for pane in &held_panes {
        registry.finish_pane_close(pane, true);
    }
    for (record, hold) in records.iter().zip(&holds) {
        let Some(record) = record else { continue };
        if let (Some(tx), Some(hold)) = (event_tx, hold) {
            surface_attach_stopped_agent(tx, record, hold.pane_id());
        }
        crate::daemon_protocol::end_unit_if_last_pane(
            record,
            registry,
            state,
            crate::dispatched_units::EndReason::Stopped,
        )
        .await;
    }
    drop(holds);
}

/// Spawn one pane via the existing registry path, tagging it with `pane_id` (so
/// the first prompt can be posted to its inbox) and optional orchestration
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
    // Issue #308: what agent this pane runs, when the caller knows something the
    // command cannot say — an orchestration role's `agent = "…"` declaration.
    // `None` means "derive it from the command", which is what a single-agent
    // schedule (no role config, so nothing to declare) always passes.
    agent_type: Option<AgentType>,
    // The `SHELL` wrapper-choice override for a command line (see
    // [`pane_env`]); `None` leaves it to the daemon's default shell.
    shell: Option<String>,
    notifier: &dyn Notifier,
) -> Result<String, SpawnError> {
    let mut env = pane_env(pane_id, false);
    if let Some(shell) = shell {
        env.push(("SHELL".to_string(), shell));
    }
    let opts = SpawnOptions {
        command,
        cwd: Some(cwd),
        display_name: Some(display_name.unwrap_or(task_name)),
        rows: 24,
        cols: 80,
        env,
        tab_membership: membership,
        // PRD #127 finding #4: tag the daemon-side registry entry with the
        // agent type inferred from the command (e.g. `claude` → `ClaudeCode`),
        // matching what `surface_spawned_pane` puts on the live card and what
        // TUI-spawned panes register (see `tab.rs`). Without this the daemon
        // stored `None`, so a scheduled card showed e.g. `claude` while live
        // but reverted to "No agent" after a reconnect rebuilt it from
        // `list_agents`. `from_command` returns `None` for bare commands, the
        // same legacy placeholder behavior.
        //
        // Issue #308: a caller-supplied type wins, so a role whose command is a
        // launcher (`devbox run -- codex`) is badged and WRAPPED from its first
        // dispatched spawn rather than reading "No agent" until its first task.
        agent_type: agent_type.or_else(|| AgentType::from_command(command)),
    };
    registry.spawn_agent(opts).map_err(|e| {
        if e.is_admission_frozen() {
            return SpawnError::DaemonRestarting;
        }
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
pub(crate) fn pane_env(pane_id: &str, pin_sh: bool) -> Vec<(String, String)> {
    let mut env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())];
    if pin_sh {
        env.push((
            "SHELL".to_string(),
            crate::platform::shell::fixed_command_shell("/bin/sh"),
        ));
    }
    env
}

/// PRD #1666 S5: post `prompt` as the first inbox message of the agent
/// `agent_id` just spawned in `pane_id`, from the deck under `reason`. An empty
/// prompt posts nothing. A refusal is logged and answered: a spawn that cannot
/// give its agent the task it was started for has failed (PR #1735 review).
pub(crate) fn post_seed(
    registry: &AgentPtyRegistry,
    pane_id: &str,
    agent_id: &str,
    reason: &str,
    prompt: &str,
) -> Result<Option<crate::inbox::MessageId>, String> {
    if prompt.trim().is_empty() {
        return Ok(None);
    }
    match crate::inbox_hub::post_to_live(
        registry,
        crate::inbox::Recipient::bound(pane_id, agent_id),
        crate::inbox::Content::from_deck(reason, "first prompt", prompt),
    ) {
        Ok(id) => {
            tracing::info!(pane_id, agent_id, message_id = %id, "spawn: first prompt posted");
            Ok(Some(id))
        }
        Err(error) => {
            tracing::warn!(pane_id, agent_id, %error, "spawn: the agent's inbox refused its first prompt");
            Err(error)
        }
    }
}

/// PR #1735 review: the refusal the inbox would give `prompt` as a first
/// prompt now, asked before a spawn starts anything, so a task the inbox would
/// refuse costs no agent. A just-spawned pane's inbox is empty, so only the
/// checks that do not depend on the recipient apply: the body's size and the
/// deck's room. An empty prompt posts nothing and is never refused.
pub(crate) fn admit_seed(
    registry: &AgentPtyRegistry,
    reason: &str,
    prompt: &str,
) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Ok(());
    }
    registry
        .inbox()
        .admit_reply(&crate::inbox::Content::from_deck(
            reason,
            "first prompt",
            prompt,
        ))
        .map_err(|error| error.to_string())
}

/// The spawn error, announced as a failed spawn, for a first prompt the inbox
/// refused.
fn seed_refused(notifier: &dyn Notifier, task: &str, error: &str) -> SpawnError {
    let message = format!("the agent's inbox refused its first prompt: {error}");
    notifier.notify(NotifyEvent::SpawnFailed {
        task: task.to_string(),
        message: message.clone(),
    });
    SpawnError::Agent(message)
}

/// PRD #1666 S5: the sender `reason` a spawn's first prompt is posted under —
/// the request's own, else a scheduled task named after it.
fn seed_reason(req: &SpawnRequest) -> String {
    req.seed_reason
        .clone()
        .unwrap_or_else(|| format!("schedule:{}", req.task_name))
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
    // Issue #308: the caller's resolved agent type for this pane, when it knows
    // one the command cannot reveal (a role's `agent = "…"`). `None` derives
    // from the command, as before. Without it a declared launcher role's
    // freshly-surfaced card read "No agent" while the daemon registry already
    // knew it was Codex, and the label only corrected itself on the pane's
    // first real hook — the exact pre-first-task blankness this issue is about.
    agent_type: Option<AgentType>,
    // `None` only on the attach path ([`surface_attach_started_agent`]), for a
    // start that named nothing and so has no friendly title to carry.
    task_name: Option<&str>,
    // Issue #1507: the registry id of the agent this card draws, for the TUI's
    // creation order only — see `SURFACED_AGENT_ID_METADATA_KEY`.
    agent_id: &str,
) {
    let mut metadata = HashMap::new();
    if let Some(task_name) = task_name {
        metadata.insert(DISPLAY_NAME_METADATA_KEY.to_string(), task_name.to_string());
    }
    metadata.insert(
        crate::event::SURFACED_AGENT_ID_METADATA_KEY.to_string(),
        agent_id.to_string(),
    );
    // Issue #684: declare that the DAEMON authored this start to draw a card,
    // rather than a producer announcing a conversation. `session_id` below is the
    // PANE ID and there is no `agent_id`, so without the marker an attached TUI's
    // `AppState` established the pane id as the pane's hook GENERATION — a
    // TUI-owned seed or role prompt bound it, and the agent's genuine
    // `SessionStart` then read as a rollover and abandoned the prompt with "the
    // agent's conversation changed". See `CARD_SURFACE_SESSION_START_ORIGIN` and
    // `prompt/pane-input/033`.
    metadata.insert(
        crate::event::SESSION_START_ORIGIN_METADATA_KEY.to_string(),
        crate::event::CARD_SURFACE_SESSION_START_ORIGIN.to_string(),
    );
    let event = AgentEvent {
        // PRD #1223: the PLACEHOLDER key, not the bare pane id, so this card and
        // a TUI-owned placeholder for the same pane are one card whichever order
        // they reach the TUI in. See `crate::state::placeholder_session_id`.
        session_id: crate::state::placeholder_session_id(pane_id),
        agent_type: agent_type
            .or_else(|| AgentType::from_command(command))
            .unwrap_or(AgentType::None),
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
        background: None,
        background_revision: None,
    };
    let _ = event_tx.send(BroadcastMsg::Event(event));
}

/// PRD #1223: surface an agent started over the ATTACH socket (`StartAgent` /
/// `StartPreparedAgent`) to every attached TUI, from the registry record the
/// spawn just published.
///
/// Before this, only the daemon-internal spawn path (`spawn` above — schedules
/// and dispatch) surfaced what it started. An attach-socket start relied on the
/// CLIENT that sent it to draw the card or tab locally, which is right for the
/// sending TUI and leaves every other client blind: a desktop-started agent was
/// registered and running while an already-attached TUI painted nothing, and a
/// desktop-started orchestration showed up only as flat cards once its roles'
/// hooks fired, with no tab and no role names (`newagent/visibility/001` /
/// `002`). The same held between two attached TUIs. The daemon owns the start,
/// so the daemon announces it, once, for every client.
///
/// What is emitted depends on the pane's membership, mirroring `spawn` above:
///
/// * no membership (a dashboard pane) — the card-surfacing `SessionStart`,
///   titled with the record's display name;
/// * an orchestration role — a ONE-role [`BroadcastMsg::OrchestrationSurface`],
///   then that card-surfacing `SessionStart` titled with the role name. An
///   attach start carries one role, so the daemon cannot know the whole set;
///   the TUI's surface consumer builds the tab from the first role and GROWS it
///   from each later one by the orchestration's identity (issue #868's path);
/// * a mode pane (the deprecated `TabMembership::Mode`, only from an older
///   TUI), or a role with no role name — nothing. There is no live mode surface
///   to publish, so a synthetic dashboard card would misfile the pane rather
///   than surface it.
///
/// Idempotent against the sending TUI, which has already built its own card
/// or tab: the `SessionStart` lands on that TUI's placeholder card under the
/// shared key ([`crate::state::placeholder_session_id`]) in either order, and
/// the surface consumer skips a surface whose pane an existing tab already owns
/// (`surface_one_orchestration`'s `already_built`). Called after the record is
/// published and any role registered, and before the start is answered, so a
/// surface never names an agent the daemon does not yet hold. Best-effort, as
/// every broadcast: with no subscriber the send errs and is ignored.
pub(crate) fn surface_attach_started_agent(
    event_tx: &broadcast::Sender<BroadcastMsg>,
    record: &crate::agent_pty::AgentRecord,
    command: Option<&str>,
) {
    let Some(pane_id) = record.pane_id_env.as_deref() else {
        // No pane id means no hook routing and no TUI pane to attach by.
        return;
    };
    let cwd = record.cwd.as_deref().unwrap_or_default();
    match record.tab_membership.as_ref() {
        None => surface_spawned_pane(
            event_tx,
            pane_id,
            cwd,
            command,
            record.agent_type.clone(),
            record.display_name.as_deref(),
            &record.id,
        ),
        Some(TabMembership::Orchestration {
            name,
            role_index,
            role_name,
            is_start_role,
            orchestration_cwd,
            display_title,
            orchestration_id,
        }) if !role_name.is_empty() => {
            let surface = crate::event::OrchestrationSurface {
                name: name.clone(),
                // The orchestration's SHARED cwd is its identity in the TUI
                // (the tab is found and grown by it); a role's own cwd may
                // differ, and is what the card below is rooted at.
                cwd: orchestration_cwd.clone().unwrap_or_else(|| cwd.to_string()),
                display_title: display_title.clone(),
                orchestration_id: orchestration_id.clone(),
                roles: vec![crate::event::OrchestrationSurfaceRole {
                    pane_id: pane_id.to_string(),
                    role_index: *role_index,
                    role_name: role_name.clone(),
                    is_start_role: *is_start_role,
                }],
                // Issue #1395: stamped onto the start role's record by the
                // caller from daemon state; `None` for every other role.
                context_path: record
                    .orchestrator_context_path
                    .clone()
                    .filter(|_| *is_start_role),
            };
            let _ = event_tx.send(BroadcastMsg::OrchestrationSurface(surface));
            // After the surface, as `spawn` orders it, so the tab exists before
            // the card it names.
            surface_spawned_pane(
                event_tx,
                pane_id,
                cwd,
                command,
                record.agent_type.clone(),
                Some(role_name),
                &record.id,
            );
        }
        Some(_) => {}
    }
}

/// PRD #1223: announce to every attached TUI that `record`'s agent was stopped
/// and its pane `pane_id` is gone — the removal half of
/// [`surface_attach_started_agent`].
///
/// `StopAgent` used to answer only the client that sent it. The sending TUI
/// cleans up locally after its own close, but every OTHER client was left with
/// what it had drawn: a desktop-stopped agent kept its card in an attached TUI,
/// and a desktop-closed orchestration kept its tab, indefinitely
/// (`newagent/visibility/003` / `004`). Emitted from the `StopAgent` arm itself,
/// so it covers every route into it — the desktop's single stop, its per-role
/// orchestration fan-out, a TUI's own `Ctrl+W`, and a TUI's attach-failure
/// cleanup — without asking which client sent it.
///
/// One card-surface-shaped `SessionEnd`, marked
/// [`crate::event::DAEMON_PANE_CLOSED_METADATA_KEY`] and filed under the
/// placeholder key as the start's card is. It names the stopped agent, so a TUI
/// never drops a pane a successor agent has since taken.
///
/// Idempotent by construction on the receiving side
/// ([`crate::state::AppState::apply_daemon_pane_closed`]): a TUI that already
/// closed the pane itself finds nothing to remove. The caller emits it only
/// after the child is reaped and the pane unregistered, and while it still holds
/// the pane's cleanup hold, so no successor's start can be broadcast ahead of
/// it. Best-effort, as every broadcast.
pub(crate) fn surface_attach_stopped_agent(
    event_tx: &broadcast::Sender<BroadcastMsg>,
    record: &crate::agent_pty::AgentRecord,
    pane_id: &str,
) {
    let mut metadata = HashMap::new();
    metadata.insert(
        crate::event::DAEMON_PANE_CLOSED_METADATA_KEY.to_string(),
        crate::event::DAEMON_PANE_CLOSED_METADATA_VALUE.to_string(),
    );
    let event = AgentEvent {
        session_id: crate::state::placeholder_session_id(pane_id),
        agent_type: record.agent_type.clone().unwrap_or(AgentType::None),
        event_type: EventType::SessionEnd,
        tool_name: None,
        tool_detail: None,
        cwd: record.cwd.clone(),
        timestamp: Utc::now(),
        user_prompt: None,
        metadata,
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(record.id.clone()),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    };
    let _ = event_tx.send(BroadcastMsg::Event(event));
}

/// Issue #960: the run-identifying tab label for a DAEMON-SPAWNED orchestration,
/// or `None` when there is nothing to add to the canonical `name`.
///
/// PRD #120 S1: disambiguate concurrent dispatched orchestration tabs. The
/// daemon-initiated path carries no user-typed title, so N concurrent issue
/// dispatches would all paint identically-labelled tabs (the shared
/// orchestration `name`, e.g. `issue-work`) — indistinguishable in the tab
/// strip. Append the per-spawn identity: the cwd basename, which for issue
/// dispatch is the per-issue worktree `issue-<n>`. `name` stays the PREFIX so
/// the canonical label reads first and survives the tab strip's
/// trailing-ellipsis truncation. When the basename already equals `name` (an
/// unnamed orchestration whose name resolved to its own cwd basename) there's
/// nothing to add — fall back to `None`, i.e. the canonical `name`.
///
/// Called ONCE per spawn, before the role loop, and its value goes to both
/// consumers: every role pane's `TabMembership::Orchestration.display_title`
/// (which survives a detach/reattach) and the `OrchestrationSurface` broadcast
/// (which paints the label live). Two computations of the same string is what
/// issue #960 was — the loop stamped `None` while only the broadcast carried the
/// title — so keep it one value with one producer.
///
/// Never returns `Some("")`: the format string always contains `" · "`, and both
/// the hydration fallback (`tab.rs`) and `validate_tab_membership` treat an
/// empty title as absent, so an empty `Some` would defeat the fallback rather
/// than carry a title.
fn dispatched_orchestration_display_title(name: &str, cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .filter(|b| b != name)
        .map(|b| format!("{name} · {b}"))
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
#[allow(clippy::too_many_arguments)]
fn surface_spawned_orchestration(
    event_tx: &broadcast::Sender<BroadcastMsg>,
    name: &str,
    cwd: &str,
    // Issue #960: computed by the caller (once, before its role loop) and
    // stamped on every role pane's membership, so the live label this broadcast
    // paints and the one a reattach rebuilds are the same value rather than two
    // computations that can drift apart.
    display_title: Option<String>,
    roles: &[RoleSpawn],
    agents: &[SpawnedAgent],
    orchestration_id: &str,
    context_path: Option<&Path>,
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
    let surface = crate::event::OrchestrationSurface {
        name: name.to_string(),
        cwd: cwd.to_string(),
        display_title,
        orchestration_id: Some(orchestration_id.to_string()),
        roles: surface_roles,
        context_path: context_path.map(|p| p.to_string_lossy().into_owned()),
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
pub(crate) fn next_pane_id(task_name: &str, role_index: Option<usize>) -> String {
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

/// Issue #770: does `pane_id` have the shape [`next_pane_id`] mints for an
/// ORCHESTRATION ROLE pane — `sched-<name>-<counter>-r<role index>`?
///
/// The recognizer for the producer directly above it, and deliberately kept
/// beside it so the two cannot drift. It answers one narrow question: is this
/// id one the DAEMON's own spawn path produced for a role of an orchestration
/// (`dispatch --orchestration`, a scheduled fire, issue dispatch)? That is the
/// population issue #770 is about — those agents can outlive the daemon that
/// spawned them, and the daemon that comes up next holds no
/// [`crate::state::AppState::pane_role_map`] entry for them, so their
/// `dot-agent-deck delegate` is refused forever while their hook events keep
/// flowing.
///
/// It is NOT a general "is this an orchestration pane" oracle, and must not be
/// read as one. A `Ctrl+n` orchestration tab's panes are numbered by the TUI
/// (`0`, `1`, `2`, …) and carry nothing that distinguishes them from a plain
/// dashboard pane, so they answer `false` here. That is the honest answer
/// rather than a gap: those panes are the TUI's, their PTYs die with the
/// daemon, and there is no surviving agent to strand.
///
/// The suffix is unambiguous by construction. `next_pane_id` appends
/// `-<counter>` for a non-role pane and `-<counter>-r<idx>` for a role pane,
/// and the name in between is sanitized to `[A-Za-z0-9_-]` — so the LAST
/// `-`-separated segment of a non-role id is always pure digits, and only a
/// role id can end in `r<digits>`. A task name that itself ends in `-r0` still
/// gets the counter appended after it, so it cannot forge the shape.
pub fn is_orchestration_role_pane_id(pane_id: &str) -> bool {
    pane_id.starts_with(SCHEDULE_PANE_ID_PREFIX)
        && pane_id
            .rsplit('-')
            .next()
            .and_then(|seg| seg.strip_prefix('r'))
            .is_some_and(|idx| !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// Tab-reuse lifecycle (PRD #127 Phase 2B, M2.2)
// ---------------------------------------------------------------------------

/// Default reuse debounce window (PRD #127 Q6). Since PRD #1666 S5 it delays
/// nothing: a reuse prompt is posted to the pane's inbox at once, and the
/// doorbell's draft guard keeps it out of a draft the user is typing.
pub const DEFAULT_REUSE_DEBOUNCE_MS: u64 = 5000;

/// The reuse debounce window (see [`DEFAULT_REUSE_DEBOUNCE_MS`]). Overridable via
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
    /// Issue #835: what this recorded tab actually IS, so a fire that explicitly
    /// asks for a different shape does not re-deliver into it. Taken from
    /// [`SpawnHandle::kind`], so it is the shape that was opened rather than the
    /// one that was requested.
    pub kind: SpawnKind,
}

/// Daemon-owned, in-memory reuse registry keyed by scheduled task `name`
/// (PRD #127 Q8). Threaded into the scheduler callback factory so each fire
/// can consult/record it. Wiped on daemon restart.
pub type ReuseRegistry = Arc<Mutex<HashMap<String, ReuseEntry>>>;

/// Construct an empty reuse registry.
pub fn new_reuse_registry() -> ReuseRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// A tab already recorded for a task name, with the identity that makes it
/// reusable.
///
/// Issue #617 (finding 2): `live: bool` used to be the whole answer, computed as
/// `registry.pane_is_live(&entry.delivery_pane_id)` — i.e. "does SOME live
/// process own that pane id right now". A pane id is a recycled handle, so a tab
/// that exited and had its `DOT_AGENT_DECK_PANE_ID` inherited by an unrelated
/// agent answered `true` and the scheduled prompt was submitted into that
/// stranger, which may then execute the task with its own tools. The field is now
/// the resolved AGENT, and it is `Some` only when the pane's current live agent is
/// one of the ones [`ReuseEntry::agent_ids`] recorded for this task — so
/// "reusable" and "who to deliver to" are one answer instead of two that can
/// disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingTab {
    pub pane_id: String,
    /// The delivery pane's current live registry agent, when it is one this task
    /// spawned. `None` when the pane is dead OR when a different agent has
    /// inherited the pane id.
    pub live_agent_id: Option<String>,
    /// Issue #835: the shape this tab was opened with. `None` for a record made
    /// before the field existed — treated as "unknown", which never blocks reuse.
    pub kind: Option<SpawnKind>,
}

/// Reuse-vs-spawn decision (pure, unit-tested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReuseDecision {
    /// Open a brand-new tab (and, for a reuse task, record it).
    SpawnFresh,
    /// Re-deliver into the existing pane, bound to the agent resolved at
    /// DECISION time, so the post is identity-guarded rather than pane-keyed.
    Reuse { pane_id: String, agent_id: String },
}

/// Decide whether a fire reuses an existing tab or spawns fresh.
/// `new_tab_per_fire == true` always spawns fresh; otherwise reuse iff a
/// recorded tab for the name still has one of its own agents live on its
/// delivery pane (a stale/closed one, or one whose pane id has been inherited by
/// an unrelated agent → fresh).
///
/// `requested` (issue #835) is the shape this fire was TOLD to open, i.e.
/// `Some` only when the task declares a `shape`. When it disagrees with the tab
/// on record, the fire spawns fresh instead of re-delivering: the recorded tab
/// is the wrong shape, and re-delivering into it would reproduce exactly the
/// surprise the `shape` field exists to remove — a user who discovers their
/// schedule is firing a team, sets `shape = "single"`, and finds the next fire
/// still landing in the old orchestrator pane.
///
/// `None` (a task with no declared shape) keeps the pre-#835 behaviour exactly:
/// the shape is derived per fire and never compared, so a config-derived task
/// reuses its tab as it always did.
pub fn decide_reuse(
    new_tab_per_fire: bool,
    existing: Option<ExistingTab>,
    requested: Option<&SpawnKind>,
) -> ReuseDecision {
    if new_tab_per_fire {
        return ReuseDecision::SpawnFresh;
    }
    match existing {
        Some(ExistingTab {
            pane_id,
            live_agent_id: Some(agent_id),
            kind,
        }) => {
            // A recorded tab with no `kind` predates the field; "unknown" never
            // blocks reuse, so this can only ever spawn fresh on a KNOWN mismatch.
            if let (Some(want), Some(have)) = (requested, kind.as_ref())
                && want != have
            {
                return ReuseDecision::SpawnFresh;
            }
            ReuseDecision::Reuse { pane_id, agent_id }
        }
        _ => ReuseDecision::SpawnFresh,
    }
}

/// The [`SpawnKind`] a resolved target will open — the request side of
/// [`decide_reuse`]'s comparison (issue #835).
fn kind_of_target(target: &SpawnTarget) -> SpawnKind {
    match target {
        SpawnTarget::SingleAgent { .. } => SpawnKind::SingleAgent,
        SpawnTarget::Orchestration { name, .. } => SpawnKind::Orchestration { name: name.clone() },
    }
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
            kind: Some(e.kind.clone()),
            // PRD #127 C3: gate reuse on the liveness of the SPECIFIC pane the
            // prompt is delivered into (orchestrator role / single-agent pane),
            // NOT "any agent for the task" — otherwise we'd re-deliver into a
            // dead orchestrator pane while a sibling role pane is still alive.
            //
            // Issue #617 (finding 2): and on that pane's live agent being one
            // THIS task spawned. `ReuseEntry` has always stored `agent_ids` and
            // this decision never consulted them, so a recycled `pane_id_env`
            // read as "the tab is still alive". Consulting them costs one
            // `contains` and turns the answer into an identity the delivery can
            // be bound to.
            //
            // The cost, stated plainly: a tab whose agent is legitimately
            // RESTARTED in place (a new registry agent on the same pane, not
            // recorded in this entry) is no longer recognised as reusable, and
            // the next fire opens a fresh tab instead of re-delivering into it.
            // That is a visible behaviour change and it is the safe direction —
            // an extra tab, versus a scheduled prompt submitted into a process
            // that never ran this task.
            live_agent_id: registry
                .pane_current_agent_id(&e.delivery_pane_id)
                .filter(|live| e.agent_ids.iter().any(|recorded| recorded == live)),
        });
        decide_reuse(
            new_tab_per_fire,
            existing,
            req.resolved_target.as_ref().map(kind_of_target).as_ref(),
        )
    };

    match decision {
        ReuseDecision::Reuse { pane_id, agent_id } => {
            // PRD #1666 S5: posted to the reused pane's live agent. No idle
            // debounce: the message waits in the inbox, and the doorbell's draft
            // guard keeps it out of a user's unsent draft.
            let _ = debounce;
            post_seed(
                registry,
                &pane_id,
                &agent_id,
                &seed_reason(&req),
                &req.prompt,
            )
            .map_err(|error| seed_refused(notifier, &req.task_name, &error))?;
            Ok(())
        }
        ReuseDecision::SpawnFresh => {
            let task_name = req.task_name.clone();
            // detach_delivery = false (no effect since PRD #1666; see `spawn`).
            let handle = spawn(req, registry, notifier, event_tx, false, state).await?;
            // Record the tab for reuse only when the task opts into reuse.
            if !new_tab_per_fire {
                let entry = ReuseEntry {
                    agent_ids: handle.agents.iter().map(|a| a.id.clone()).collect(),
                    delivery_pane_id: handle.delivery_pane_id.clone(),
                    // What was actually opened, not what was asked for.
                    kind: handle.kind.clone(),
                };
                reuse.lock().unwrap().insert(task_name, entry);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;
    use std::time::Instant;

    fn spawn_byte_target(registry: &Arc<AgentPtyRegistry>, pane_id: &str) -> String {
        #[cfg(unix)]
        let command = "/bin/cat";
        #[cfg(windows)]
        let command = "more.com";
        registry
            .spawn_agent(SpawnOptions {
                command: Some(command),
                // Pinned endpoints: these targets are bare byte sinks that emit
                // nothing, and must not resolve a route to a real deck either
                // (`crate::test_isolation`).
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    pane_id.to_string(),
                )]),
                ..SpawnOptions::default()
            })
            .expect("spawn byte-observation target")
    }

    fn parse_config(toml: &str) -> ProjectConfig {
        toml::from_str(toml).expect("parse project config")
    }

    // Issue #835 — the `shape` STRING vocabulary. One parser serves the config
    // loader, both CLI write doors and the daemon's fire-time resolve, so these
    // cases pin what every one of them accepts.
    #[test]
    fn shape_parse_accepts_the_three_documented_forms() {
        assert_eq!(
            SpawnShapeOverride::parse("single").unwrap(),
            SpawnShapeOverride::SingleAgent
        );
        assert_eq!(
            SpawnShapeOverride::parse("orchestration").unwrap(),
            SpawnShapeOverride::Orchestration(None)
        );
        assert_eq!(
            SpawnShapeOverride::parse("orchestration:review").unwrap(),
            SpawnShapeOverride::Orchestration(Some("review".to_string()))
        );
    }

    // Surrounding whitespace is tolerated on both the value and the name, since
    // it arrives from hand-edited TOML and a shell quoting a `--shape` argument.
    #[test]
    fn shape_parse_trims_surrounding_whitespace() {
        assert_eq!(
            SpawnShapeOverride::parse("  single  ").unwrap(),
            SpawnShapeOverride::SingleAgent
        );
        assert_eq!(
            SpawnShapeOverride::parse("orchestration:  review ").unwrap(),
            SpawnShapeOverride::Orchestration(Some("review".to_string()))
        );
    }

    // Anything else is an ERROR, never a silent "use the default" — the whole
    // point of the field is that a shape the author wrote is the shape that
    // fires. The message names the offending value and lists the vocabulary,
    // because all three callers surface it to a human.
    #[test]
    fn shape_parse_rejects_everything_else() {
        for bad in [
            "",
            "   ",
            "Single",
            "SINGLE",
            "team",
            "orchestration:",
            "orchestration:   ",
            "orchestrations",
            "orchestration review",
            "dispatcher",
        ] {
            let err = match SpawnShapeOverride::parse(bad) {
                Ok(v) => panic!("shape {bad:?} must be rejected, got {v:?}"),
                Err(e) => e,
            };
            assert!(
                err.contains("shape"),
                "error for {bad:?} must name the field, got {err:?}"
            );
            assert!(
                err.contains("single") || err.contains("name after the colon"),
                "error for {bad:?} must state what IS accepted, got {err:?}"
            );
            // These messages are printed straight to a terminal by `schedule
            // add`/`update` and into the daemon log by a failed fire. A `\`
            // line-continuation in the literal survived `cargo fmt` as a run of
            // real spaces once already, so pin the rendered shape rather than
            // trusting the source to keep its own indentation out of the string.
            // Checked only for inputs that carry no run of spaces themselves —
            // the message echoes the offending value, so `"   "` would otherwise
            // fail on the user's own text rather than on the literal.
            if !bad.contains("  ") {
                assert!(
                    !err.contains("  "),
                    "error for {bad:?} must not carry source indentation, got {err:?}"
                );
            }
        }
    }

    // A parsed shape drives the SAME resolver `dispatch` uses, so the end-to-end
    // property the scheduler relies on is: `single` beats a dir that defines
    // orchestrations, rather than merely parsing.
    #[test]
    fn shape_parse_single_then_resolve_beats_a_dir_defining_orchestrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"team\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n",
        )
        .expect("write config");
        let cfg = load_config_for_dir(dir.path());
        assert!(
            matches!(
                decide_target(cfg.as_ref(), dir.path(), Some("mycmd")),
                SpawnTarget::Orchestration { .. }
            ),
            "precondition: with no shape this dir derives an orchestration"
        );
        let over = SpawnShapeOverride::parse("single").expect("parse single");
        assert_eq!(
            decide_target_with_override(cfg.as_ref(), dir.path(), Some("mycmd"), Some(&over))
                .expect("single always resolves"),
            SpawnTarget::SingleAgent {
                command: Some("mycmd".to_string()),
                inherited: None,
            },
            "`single` must win over the dir's orchestrations AND carry the command"
        );
    }

    #[test]
    fn decide_target_single_agent_when_no_config() {
        let dir = Path::new("/tmp/x");
        let t = decide_target(None, dir, Some("claude"));
        assert_eq!(
            t,
            SpawnTarget::SingleAgent {
                command: Some("claude".to_string()),
                inherited: None,
            }
        );
    }

    #[test]
    fn decide_target_single_agent_none_command_means_shell() {
        // `None` command flows through to the spawn path's `$SHELL` fallback.
        let dir = Path::new("/tmp/x");
        let t = decide_target(None, dir, None);
        assert_eq!(
            t,
            SpawnTarget::SingleAgent {
                command: None,
                inherited: None,
            }
        );
    }

    #[test]
    fn decide_target_single_agent_when_config_has_no_orchestrations() {
        let cfg = parse_config("[[modes]]\nname = \"dev\"\n");
        let dir = Path::new("/tmp/x");
        let t = decide_target(Some(&cfg), dir, Some("cat"));
        assert_eq!(
            t,
            SpawnTarget::SingleAgent {
                command: Some("cat".to_string()),
                inherited: None,
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

    // --- Issue #704: ONE default rule, shared by both paths ---

    /// A roleless placeholder in slot 0 with a spawnable block behind it. This is
    /// the config that made the two paths disagree.
    fn roleless_slot_zero_config() -> ProjectConfig {
        parse_config(
            "[[orchestrations]]\nname = \"placeholder\"\nroles = []\n\n\
             [[orchestrations]]\nname = \"real\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"sh\"\n",
        )
    }

    /// The SCHEDULER path's half of the bug: `decide_target` looked only at
    /// `orchestrations.first()`, so a roleless slot 0 sent a scheduled fire to a
    /// single-agent card while `--list-targets` was offering `real` and a bare
    /// dispatch was spawning it.
    #[test]
    fn decide_target_skips_a_roleless_slot_zero_instead_of_degrading_to_one_agent() {
        let cfg = roleless_slot_zero_config();
        let dir = Path::new("/tmp/x");
        match decide_target(Some(&cfg), dir, Some("claude")) {
            SpawnTarget::Orchestration { name, roles, .. } => {
                assert_eq!(name, "real");
                assert_eq!(roles.len(), 2);
            }
            SpawnTarget::SingleAgent { .. } => panic!(
                "a scheduled fire must open the repo's spawnable orchestration, not fall through \
                 to a single agent because slot 0 happens to be empty"
            ),
        }
    }

    /// The two paths must not just each be right — they must be right for the
    /// SAME reason, which is what a shared resolver buys. Asserted over the
    /// configs that used to separate them.
    #[test]
    fn scheduler_and_bare_dispatch_agree_on_the_default_orchestration() {
        let dir = Path::new("/tmp/x");
        let declared_last = parse_config(
            "[[orchestrations]]\nname = \"first\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations]]\nname = \"chosen\"\ndefault = true\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"sh\"\nstart = true\n",
        );
        for cfg in [
            roleless_slot_zero_config(),
            two_orchestration_config(),
            declared_last,
        ] {
            let scheduled = decide_target(Some(&cfg), dir, Some("claude"));
            let bare = decide_target_with_override(
                Some(&cfg),
                dir,
                Some("claude"),
                Some(&SpawnShapeOverride::Orchestration(None)),
            )
            .expect("the bare form must resolve every one of these");
            assert_eq!(
                scheduled, bare,
                "a scheduled fire and a bare `--orchestration=` dispatch into the same repo must \
                 open the same thing — two rules for one question is the bug, whatever either \
                 rule says"
            );
        }
    }

    /// The declaration beats position on BOTH paths, which is what makes it a
    /// declaration rather than a hint.
    #[test]
    fn declared_default_wins_over_file_order_on_both_paths() {
        let cfg = parse_config(
            "[[orchestrations]]\nname = \"first\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations]]\nname = \"declared\"\ndefault = true\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"sh\"\nstart = true\n",
        );
        let dir = Path::new("/tmp/x");
        for target in [
            decide_target(Some(&cfg), dir, Some("claude")),
            decide_target_with_override(
                Some(&cfg),
                dir,
                Some("claude"),
                Some(&SpawnShapeOverride::Orchestration(None)),
            )
            .expect("bare form resolves"),
        ] {
            match target {
                SpawnTarget::Orchestration { name, .. } => assert_eq!(name, "declared"),
                other => panic!("expected the declared orchestration, got {other:?}"),
            }
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
                command: Some("claude".to_string()),
                inherited: None,
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

    /// Issue #1396 item 4: two ROLE-BEARING declarations under the requested
    /// name are refused as ambiguous rather than resolved to the first, as
    /// `crate::project_resolve::find_orchestration` refuses them for
    /// `PrepareOrchestration`. The control is a roleless namesake beside one
    /// role-bearing entry, which is not ambiguous: it cannot launch anything,
    /// so the role-bearing one is still the only target with that name.
    #[test]
    fn shape_override_refuses_a_name_two_role_bearing_orchestrations_share() {
        let dir = Path::new("/tmp/x");
        let ambiguous = parse_config(
            "[[orchestrations]]\nname = \"dup\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations]]\nname = \"dup\"\n\n\
             [[orchestrations.roles]]\nname = \"lead\"\ncommand = \"sh\"\nstart = true\n",
        );
        let err = decide_target_with_override(
            Some(&ambiguous),
            dir,
            None,
            Some(&SpawnShapeOverride::Orchestration(Some("dup".into()))),
        )
        .expect_err("a name two role-bearing orchestrations share must not resolve to the first");
        assert!(
            err.starts_with(crate::daemon_protocol::PROJECT_ERR_AMBIGUOUS_ORCHESTRATION),
            "the refusal must carry the ambiguous-orchestration code: {err}"
        );

        let roleless_namesake = parse_config(
            "[[orchestrations]]\nname = \"dup\"\nroles = []\n\n\
             [[orchestrations]]\nname = \"dup\"\n\n\
             [[orchestrations.roles]]\nname = \"lead\"\ncommand = \"sh\"\nstart = true\n",
        );
        match decide_target_with_override(
            Some(&roleless_namesake),
            dir,
            None,
            Some(&SpawnShapeOverride::Orchestration(Some("dup".into()))),
        ) {
            Ok(SpawnTarget::Orchestration { name, roles, .. }) => {
                assert_eq!(name, "dup");
                assert_eq!(roles[0].role_name, "lead");
            }
            other => panic!("control: a roleless namesake is not ambiguous, got {other:?}"),
        }
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
                agent_type: None,
                role_index: 0,
                role_name: "worker".into(),
                command: "sh".into(),
                is_start_role: false,
            },
            RoleSpawn {
                agent_type: None,
                role_index: 1,
                role_name: "orchestrator".into(),
                command: "cat".into(),
                is_start_role: false,
            },
        ];
        assert_eq!(orchestrator_role_index(&roles), 1);
    }

    /// Issue #523: `start = true` is the declaration and the name only a
    /// fallback, so a flagged role beside a differently positioned role NAMED
    /// `orchestrator` is the orchestrator — the answer the `Ctrl+n` tab and
    /// the desktop give for the same config.
    #[test]
    fn orchestrator_role_index_start_flag_outranks_the_name() {
        let roles = vec![
            RoleSpawn {
                agent_type: None,
                role_index: 0,
                role_name: "orchestrator".into(),
                command: "sh".into(),
                is_start_role: false,
            },
            RoleSpawn {
                agent_type: None,
                role_index: 1,
                role_name: "lead".into(),
                command: "cat".into(),
                is_start_role: true,
            },
        ];
        assert_eq!(orchestrator_role_index(&roles), 1);
    }

    #[test]
    fn orchestrator_role_index_falls_back_to_start_role_then_first() {
        let start_role = vec![
            RoleSpawn {
                agent_type: None,
                role_index: 0,
                role_name: "lead".into(),
                command: "sh".into(),
                is_start_role: false,
            },
            RoleSpawn {
                agent_type: None,
                role_index: 1,
                role_name: "boss".into(),
                command: "cat".into(),
                is_start_role: true,
            },
        ];
        assert_eq!(orchestrator_role_index(&start_role), 1);

        let neither = vec![RoleSpawn {
            agent_type: None,
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

    /// Issue #600: an orchestration spawn is ALL-OR-NOTHING — an `Err` from
    /// `spawn` means nothing it started is still running, and nothing it
    /// registered is still registered.
    ///
    /// Role 1 is a command that cannot be exec'd, so the failure is the loop's
    /// real early return rather than a simulated one.
    ///
    /// **This test's assertion is the INVERSE of the one it carried for issue
    /// #454, and deliberately so.** #454 moved role registration from after the
    /// loop to inside it, and pinned that by asserting the surviving role of a
    /// partial failure was still in `pane_role_map`. It said in as many words
    /// what it did not assert: "role 0's child is still running afterwards and
    /// the caller gets no handle with which to close it … tracked separately."
    /// #600 is that separate track, and it decided the orphan is the defect: the
    /// loop now tears down what it started, which unregisters those same panes.
    /// So the end state flips from "one role registered" to "none", and the
    /// property under test flips from *ordering* to *atomicity*.
    ///
    /// #454's ordering is still load-bearing and is still stated where it lives
    /// (the comment on the `register_orchestration_role` call): the rollback
    /// unregisters a role only after that role's child is closed, so a live child
    /// is never missing its routing entry. What is no longer OBSERVABLE from
    /// here is the distinction between registering inside the loop and after it —
    /// under atomicity both end with an empty map. The success-path consequence
    /// of that ordering keeps its own coverage in `orchestration/dispatch/001`
    /// (a dispatched orchestrator's `delegate` actually routes).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_partial_orchestration_spawn_leaves_nothing_running_or_registered() {
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
            agent_type: None,
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
                    default: false,
                    name: "partial-454".to_string(),
                    roles: vec![OrchestrationRoleConfig {
                        agent: None,
                        name: "orchestrator".to_string(),
                        command: "/bin/sh".to_string(),
                        start: true,
                        description: None,
                        prompt_template: None,
                        clear: true,
                    }],
                }),
            }),
            compose_orchestrator_context: None,
            seed_reason: None,
        };

        let result = spawn(req, &registry, &SilentNotifier, None, false, Some(&state)).await;
        assert!(
            result.is_err(),
            "precondition: role 1 must fail to spawn, aborting the orchestration"
        );

        // No `SpawnHandle` came back and no orchestration tab was ever surfaced
        // (the broadcast sits past the early return), so anything still live here
        // is unreachable by every close path the user has.
        let live = registry.agent_records();
        assert!(
            live.is_empty(),
            "role 0 spawned before role 1 failed; it must be torn down, not orphaned. \
             Still live: {:?}",
            live.iter()
                .map(|r| r.display_name.clone())
                .collect::<Vec<_>>()
        );

        let guard = state.read().await;
        assert!(
            guard.pane_role_map.is_empty(),
            "the rolled-back role must leave no role-map entry: {:?}",
            guard.pane_role_map
        );
        assert!(
            guard.orchestrator_pane_ids.is_empty(),
            "…nor an orchestrator marker: {:?}",
            guard.orchestrator_pane_ids
        );
        assert!(
            guard.pane_orchestration_map.is_empty(),
            "…nor a routing identity: {:?}",
            guard.pane_orchestration_map
        );
        // Issue #962: the run title recorded beside the role maps goes with them.
        assert!(
            guard.orchestration_titles.is_empty(),
            "…nor a recorded run title: {:?}",
            guard.orchestration_titles
        );
        drop(guard);

        registry.shutdown_all();
    }

    /// A two-role dispatched orchestration (`orchestrator`, then `worker`),
    /// both `sleep 30`, rooted at `dir`.
    #[cfg(unix)]
    fn closable_orchestration_request(dir: &Path, task: &str) -> SpawnRequest {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
        let role = |idx: usize, name: &str| RoleSpawn {
            agent_type: None,
            role_index: idx,
            role_name: name.to_string(),
            command: "sleep 30".to_string(),
            is_start_role: idx == 0,
        };
        SpawnRequest {
            task_name: task.to_string(),
            working_dir: dir.to_string_lossy().into_owned(),
            command: None,
            prompt: "unused".to_string(),
            resolved_target: Some(SpawnTarget::Orchestration {
                name: task.to_string(),
                roles: vec![role(0, "orchestrator"), role(1, "worker")],
                config: Box::new(OrchestrationConfig {
                    default: false,
                    name: task.to_string(),
                    roles: vec![OrchestrationRoleConfig {
                        agent: None,
                        name: "orchestrator".to_string(),
                        command: "sleep 30".to_string(),
                        start: true,
                        description: None,
                        prompt_template: None,
                        clear: false,
                    }],
                }),
            }),
            compose_orchestrator_context: None,
            seed_reason: None,
        }
    }

    /// PR #1735 third review: a dispatched agent whose first prompt is refused
    /// after the spawn surfaced its card. The deck fills while the spawn is
    /// paused just before publishing, so the card goes out and then the seed
    /// post is refused. The rollback must take the card back off every attached
    /// client — one pane-closed announcement for that pane and agent, after the
    /// card — end the unit it recorded, and leave nothing running.
    #[tokio::test]
    async fn a_dispatched_agent_whose_seed_is_refused_after_its_card_is_announced_closed() {
        struct SilentNotifier;
        impl Notifier for SilentNotifier {
            fn notify(&self, _event: NotifyEvent) {}
        }
        let dir = tempfile::tempdir().expect("tempdir for the agent cwd");
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: crate::state::SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _keep) = broadcast::channel(4096);
        let mut events = event_tx.subscribe();
        let (reached, release) = registry.pause_at_for_test("dispatch-publication");
        let spawned = {
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            let req = SpawnRequest {
                task_name: "seed-refused".to_string(),
                working_dir: dir.path().to_string_lossy().into_owned(),
                command: Some("sleep 30".to_string()),
                prompt: "the task this agent was dispatched for".to_string(),
                resolved_target: Some(SpawnTarget::SingleAgent {
                    command: Some("sleep 30".to_string()),
                    inherited: None,
                }),
                compose_orchestrator_context: None,
                seed_reason: None,
            };
            let origin = crate::dispatched_units::UnitOrigin {
                name: "seed-refused".to_string(),
                worktree: dir.path().to_path_buf(),
                branch: "agent/seed-refused".to_string(),
                clone_dir: dir.path().to_path_buf(),
                dispatcher: crate::dispatched_units::Dispatcher {
                    pane_id: "disp".to_string(),
                    agent_id: "nobody".to_string(),
                },
            };
            let caller = crate::dispatch_return::DispatchCaller {
                pane_id: "disp".to_string(),
                agent_id: "nobody".to_string(),
                unit_name: "seed-refused".to_string(),
            };
            tokio::spawn(async move {
                spawn_dispatched_unit(
                    req,
                    &registry,
                    &SilentNotifier,
                    Some(&event_tx),
                    true,
                    Some(&state),
                    origin,
                    caller,
                )
                .await
                .map(|handle| handle.unit_id)
            })
        };
        reached.await.expect("the dispatch reaches the pause");
        let limits = registry.inbox().limits();
        'fill: for pane in 0.. {
            let to = crate::inbox::Recipient::bound(format!("filler-{pane}"), "filler");
            for n in 0..limits.max_per_recipient {
                if registry
                    .inbox()
                    .post_direct(
                        to.clone(),
                        crate::inbox::Content::from_deck("test", format!("filler {n}"), "x"),
                    )
                    .is_err()
                {
                    break 'fill;
                }
            }
        }
        let _ = release.send(());
        let outcome = tokio::time::timeout(Duration::from_secs(15), spawned)
            .await
            .expect("the spawn answers")
            .expect("the spawn task");
        let Err(SpawnError::Agent(message)) = outcome else {
            panic!("a spawn that lost its seed fails: {outcome:?}");
        };
        assert!(message.contains("first prompt"), "{message}");

        let mut surfaced = None;
        let mut closed = Vec::new();
        while let Ok(msg) = events.try_recv() {
            if let BroadcastMsg::Event(event) = msg {
                if event.is_daemon_pane_closed() {
                    closed.push(event);
                } else if event.event_type == EventType::SessionStart && surfaced.is_none() {
                    surfaced = Some(event);
                }
            }
        }
        let surfaced = surfaced.expect("the spawn surfaced the agent's card");
        let [closure] = closed.as_slice() else {
            panic!("expected exactly one pane-closed announcement, got {closed:#?}");
        };
        assert_eq!(closure.pane_id, surfaced.pane_id);
        assert!(closure.agent_id.is_some(), "the closure names the agent");
        assert_eq!(registry.live_count(), 0, "nothing is left running");
        assert_eq!(
            registry.dispatched_units().live().count(),
            0,
            "the unit the spawn recorded has ended"
        );
    }

    /// Start [`closable_orchestration_request`] as a dispatched unit, pause it
    /// at `pause`, close its orchestration through its already-registered
    /// orchestrator pane, resume it, and check that it publishes nothing for
    /// the closed panes: no tab, no card, no unit record (auditor S1).
    #[cfg(unix)]
    async fn close_a_dispatch_paused_at(pause: &str) {
        struct SilentNotifier;
        impl Notifier for SilentNotifier {
            fn notify(&self, _event: NotifyEvent) {}
        }
        let dir = tempfile::tempdir().expect("tempdir for the orchestration cwd");
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: crate::state::SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let (event_tx, _keep) = broadcast::channel(1024);
        let worktrees = crate::issue_dispatch_run::new_worktree_registry();
        let mut events = event_tx.subscribe();
        let (reached, release) = registry.pause_at_for_test(pause);
        let spawned = {
            let registry = registry.clone();
            let state = state.clone();
            let event_tx = event_tx.clone();
            let req = closable_orchestration_request(dir.path(), "closed-s1");
            let origin = crate::dispatched_units::UnitOrigin {
                name: "closed-s1".to_string(),
                worktree: dir.path().to_path_buf(),
                branch: "agent/closed-s1".to_string(),
                clone_dir: dir.path().to_path_buf(),
                dispatcher: crate::dispatched_units::Dispatcher {
                    pane_id: "disp".to_string(),
                    agent_id: "nobody".to_string(),
                },
            };
            let caller = crate::dispatch_return::DispatchCaller {
                pane_id: "disp".to_string(),
                agent_id: "nobody".to_string(),
                unit_name: "closed-s1".to_string(),
            };
            tokio::spawn(async move {
                spawn_dispatched_unit(
                    req,
                    &registry,
                    &SilentNotifier,
                    Some(&event_tx),
                    true,
                    Some(&state),
                    origin,
                    caller,
                )
                .await
                .map(|handle| handle.unit_id)
            })
        };
        reached.await.expect("the dispatch reaches the pause");
        let records = registry.agent_records();
        let pane_of = |role: &str| {
            records
                .iter()
                .find(|r| {
                    matches!(&r.tab_membership,
                        Some(TabMembership::Orchestration { role_name, .. }) if role_name == role)
                })
                .and_then(|r| r.pane_id_env.clone())
                .expect("the role was published")
        };
        let (orchestrator, worker) = (pane_of("orchestrator"), pane_of("worker"));
        assert!(
            state
                .read()
                .await
                .pane_orchestration_map
                .contains_key(&orchestrator),
            "precondition: the sibling is registered"
        );
        let report = crate::close_agents::handle_close_agents(
            crate::daemon_protocol::CloseSelector::OrchestrationOf {
                pane_id: orchestrator.clone(),
            },
            None,
            false,
            false,
            &registry,
            &state,
            &event_tx,
            &worktrees,
        )
        .await;
        assert_eq!(
            report.targets[0].outcome,
            crate::daemon_protocol::CloseOutcome::Closed,
            "{:?}",
            report.targets[0]
        );
        let _ = release.send(());
        let result = spawned.await.unwrap();
        assert!(
            matches!(&result, Err(SpawnError::Agent(m)) if m == CLOSED_WHILE_STARTING),
            "a dispatch closed while it was starting answers so: {result:?}"
        );
        assert_eq!(
            registry.dispatched_units().live().count(),
            0,
            "no live unit is recorded for a closed instance"
        );
        assert!(registry.agent_records().is_empty());
        let guard = state.read().await;
        assert!(!guard.pane_orchestration_map.contains_key(&orchestrator));
        assert!(!guard.pane_orchestration_map.contains_key(&worker));
        drop(guard);
        while let Ok(msg) = events.try_recv() {
            match msg {
                BroadcastMsg::OrchestrationSurface(surface) => {
                    panic!("a tab was surfaced for a closed orchestration: {surface:?}")
                }
                BroadcastMsg::Event(event)
                    if event.event_type == EventType::SessionStart
                        && [&orchestrator, &worker]
                            .iter()
                            .any(|p| event.pane_id.as_deref() == Some(p.as_str())) =>
                {
                    panic!("a card was announced for a closed pane: {event:?}")
                }
                _ => {}
            }
        }
        registry.shutdown_all();
    }

    /// Scenario: auditor S1 — a dispatched orchestration has published its
    /// final role and pauses before registering it. A person closes the
    /// orchestration through the already-registered orchestrator's pane. When
    /// the dispatch resumes it registers nothing, surfaces no tab and no card
    /// for the closed panes, records no unit, and answers that it was closed.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dispatch_closed_before_its_last_role_registers_publishes_nothing() {
        close_a_dispatch_paused_at("dispatch-role:worker").await;
    }

    /// Scenario: auditor S1 — the same, with the close landing after the last
    /// role registered and before the dispatch surfaces its tab and cards.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dispatch_closed_before_its_publication_publishes_nothing() {
        close_a_dispatch_paused_at("dispatch-publication").await;
    }

    // --- issue #1065: an unpublishable coordinator context refuses the spawn ---

    /// Build an orchestration `SpawnRequest` rooted at `cwd` that asks for a
    /// composed coordinator context, with `template` as the start role's own
    /// `prompt_template`.
    ///
    /// Both role commands are ones that WOULD start — `/bin/sh` is what the
    /// partial-spawn test above uses for its successful role. That is the point:
    /// these tests assert the refusal beat the role loop to it, so a live pane
    /// afterwards means the refusal landed too late rather than that a command
    /// was bad.
    fn unpublishable_orchestration_request(cwd: &Path, template: Option<String>) -> SpawnRequest {
        use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};

        let declared = |name: &str, start: bool, tpl: Option<String>| OrchestrationRoleConfig {
            agent: None,
            name: name.to_string(),
            command: "/bin/sh".to_string(),
            start,
            description: Some("Does the work".to_string()),
            prompt_template: tpl,
            clear: false,
        };
        SpawnRequest {
            task_name: "dispatch-refusal-1065".to_string(),
            working_dir: cwd.to_string_lossy().into_owned(),
            command: None,
            prompt: "Implement PRD #742 end to end.".to_string(),
            resolved_target: Some(SpawnTarget::Orchestration {
                name: "refusal-1065".to_string(),
                roles: vec![
                    RoleSpawn {
                        agent_type: None,
                        role_index: 0,
                        role_name: "orchestrator".to_string(),
                        command: "/bin/sh".to_string(),
                        is_start_role: true,
                    },
                    RoleSpawn {
                        agent_type: None,
                        role_index: 1,
                        role_name: "worker".to_string(),
                        command: "/bin/sh".to_string(),
                        is_start_role: false,
                    },
                ],
                config: Box::new(OrchestrationConfig {
                    default: false,
                    name: "refusal-1065".to_string(),
                    roles: vec![
                        declared("orchestrator", true, template),
                        declared("worker", false, None),
                    ],
                }),
            }),
            compose_orchestrator_context: Some(crate::orchestrator_context::Attendance::Unattended),
            seed_reason: None,
        }
    }

    /// A [`Notifier`] that records what it was told, so a test can assert the
    /// refusal was ANNOUNCED and not only returned.
    #[derive(Default)]
    struct RecordingNotifier(std::sync::Mutex<Vec<NotifyEvent>>);

    impl Notifier for RecordingNotifier {
        fn notify(&self, event: NotifyEvent) {
            self.0.lock().expect("notifier mutex").push(event);
        }
    }

    /// PR #1735 review: a fire whose first prompt the agent's inbox would
    /// refuse — here, one over the inbox's body limit — is a failed spawn:
    /// `SpawnFailed` is announced, the spawn answers `Err`, and no agent is
    /// left running without its task.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_first_prompt_the_inbox_refuses_fails_the_spawn() {
        let dir = crate::test_temp::tempdir().expect("tempdir for the fire's cwd");
        let registry = Arc::new(AgentPtyRegistry::new());
        let notifier = RecordingNotifier::default();
        let request = SpawnRequest {
            task_name: "seed-refused".to_string(),
            working_dir: dir.path().to_string_lossy().into_owned(),
            command: Some("cat".to_string()),
            prompt: "x".repeat(crate::inbox::DEFAULT_MAX_BODY_BYTES + 1),
            resolved_target: Some(SpawnTarget::SingleAgent {
                command: Some("cat".to_string()),
                inherited: None,
            }),
            compose_orchestrator_context: None,
            seed_reason: None,
        };
        let outcome = spawn(request, &registry, &notifier, None, true, None).await;
        let seen = notifier.0.lock().expect("notifier mutex").clone();
        let records = registry.agent_records().len();
        registry.shutdown_all();
        assert!(outcome.is_err(), "a spawn whose task was refused must fail");
        assert!(
            seen.iter()
                .any(|e| matches!(e, NotifyEvent::SpawnFailed { .. })),
            "and say so: {seen:?}"
        );
        assert_eq!(records, 0, "no agent is left running without its task");
    }

    /// Scenario: a scheduled single-agent fire lands while a daemon restart
    /// holds its admission freeze. The spawn comes back as "the daemon is
    /// restarting" — not as a failed spawn: no `SpawnFailed` is announced and
    /// nothing is started — and once the restart is called off the same fire
    /// starts its agent (PRD #1487 re-check, reviewer R3).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fire_refused_by_a_restart_is_deferred_not_failed() {
        let dir = crate::test_temp::tempdir().expect("tempdir for the fire's cwd");
        let registry = Arc::new(AgentPtyRegistry::new());
        let notifier = RecordingNotifier::default();
        let request = || SpawnRequest {
            task_name: "deferred-1487".to_string(),
            working_dir: dir.path().to_string_lossy().into_owned(),
            command: Some("cat".to_string()),
            prompt: "unused".to_string(),
            resolved_target: Some(SpawnTarget::SingleAgent {
                command: Some("cat".to_string()),
                inherited: None,
            }),
            compose_orchestrator_context: None,
            seed_reason: None,
        };

        let reservation = registry
            .freeze_admission()
            .await
            .expect("nothing in flight");
        let err = match spawn(request(), &registry, &notifier, None, true, None).await {
            Ok(_) => panic!("a spawn under a restart's reservation must be refused"),
            Err(e) => e,
        };
        assert!(
            matches!(err, SpawnError::DaemonRestarting),
            "the refusal is classified as a restart, not a failure: {err:?}"
        );
        assert!(
            err.to_string().contains("the daemon is restarting"),
            "and still says why: {err}"
        );
        let seen = notifier.0.lock().expect("notifier mutex").clone();
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, NotifyEvent::SpawnFailed { .. })),
            "a deferred fire is not announced as a failed spawn: {seen:?}"
        );
        assert!(registry.agent_records().is_empty(), "nothing was started");

        // The restart is called off: the waiter says so, and the fire starts.
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.wait_for_admission().await })
        };
        drop(reservation);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(30), waiter)
                .await
                .expect("the waiter resolves")
                .unwrap(),
            "a released freeze re-admits"
        );
        if spawn(request(), &registry, &notifier, None, true, None)
            .await
            .is_err()
        {
            panic!("the retried fire starts its agent");
        }
        assert_eq!(registry.agent_records().len(), 1);

        // A restart that goes ahead: the waiter reports the daemon going down.
        registry
            .freeze_admission()
            .await
            .expect("nothing in flight")
            .keep();
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.wait_for_admission().await })
        };
        {
            let registry = registry.clone();
            tokio::task::spawn_blocking(move || {
                registry.shutdown_all_graceful(std::time::Duration::from_millis(200))
            })
            .await
            .unwrap();
        }
        assert!(
            !tokio::time::timeout(std::time::Duration::from_secs(30), waiter)
                .await
                .expect("the waiter resolves")
                .unwrap(),
            "an accepted restart leaves the fire to the successor"
        );
    }

    /// Issue #1065: an orchestration whose coordinator context cannot be
    /// published is REFUSED — no role is started, nothing is registered, and the
    /// caller is handed the reason.
    ///
    /// This used to spawn the whole team and hand the orchestrator
    /// `req.prompt` verbatim: no role template, no `## Available agents`, no
    /// `## Delegation protocol`. The surfaces a user reads — the pane labels,
    /// the role cards, `daemon status` — looked exactly like a working team
    /// while the first role implemented the task solo and the rest idled, and
    /// the daemon log said it in one `warn!` line.
    ///
    /// **The trigger here is deliberately not the mode-0775 condition the field
    /// report hit** (issue #1047 / #329 §2, being fixed in parallel). Verifying
    /// this against a directory mode would stop reproducing the moment that
    /// lands, while proving nothing about the fallback, which is reachable
    /// through every other `ContextPublishError`. A `.dot-agent-deck` that is a
    /// regular FILE fails `open_context_dir`'s `O_DIRECTORY` open, so
    /// `refuse_a_writable_context_dir` — the mode check #1047 is about — is never
    /// reached at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unpublishable_coordinator_context_refuses_the_spawn_and_starts_no_role() {
        let dir = crate::test_temp::tempdir().expect("tempdir for the orchestration cwd");
        // `.dot-agent-deck` occupied by a regular file: the publish can neither
        // create the directory nor open what is there as one.
        std::fs::write(dir.path().join(".dot-agent-deck"), b"not a directory")
            .expect("occupy the context dir path");

        let registry = Arc::new(AgentPtyRegistry::new());
        let state: crate::state::SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let notifier = RecordingNotifier::default();

        // `SpawnHandle` carries a `Box<dyn FnOnce>` and so cannot be `Debug`;
        // matched rather than `expect_err`'d for that reason alone.
        let err = match spawn(
            unpublishable_orchestration_request(dir.path(), None),
            &registry,
            &notifier,
            None,
            false,
            Some(&state),
        )
        .await
        {
            Ok(_) => {
                registry.shutdown_all();
                panic!(
                    "an orchestration with no publishable context must be refused, not spawned \
                     with the orchestrator holding the bare task"
                );
            }
            Err(e) => e,
        };

        assert!(
            matches!(
                err,
                SpawnError::OrchestratorContext(
                    crate::orchestrator_context::ContextPublishError::ContextDirUnusable(_)
                )
            ),
            "the refusal must carry the publish cause so the caller can name the remedy, got: \
             {err:?}"
        );

        // Nothing started. Distinct from the partial-spawn test above, which
        // asserts the same emptiness after a TEARDOWN: here the refusal precedes
        // the role loop, so a live record would mean the check landed too late.
        let live = registry.agent_records();
        assert!(
            live.is_empty(),
            "no role may start when the coordinator context was refused; still live: {:?}",
            live.iter()
                .map(|r| r.display_name.clone())
                .collect::<Vec<_>>()
        );
        let guard = state.read().await;
        assert!(
            guard.pane_role_map.is_empty()
                && guard.orchestrator_pane_ids.is_empty()
                && guard.pane_orchestration_map.is_empty(),
            "a refused orchestration must register nothing: roles {:?}, orchestrators {:?}, \
             routing {:?}",
            guard.pane_role_map,
            guard.orchestrator_pane_ids,
            guard.pane_orchestration_map
        );
        drop(guard);

        // …and it was ANNOUNCED, through the same notification an abandoned
        // spawn already uses, rather than only returned.
        let seen = notifier.0.lock().expect("notifier mutex").clone();
        let announced = seen.iter().any(|e| {
            matches!(
                e,
                NotifyEvent::SpawnFailed { task, message }
                    if task == "dispatch-refusal-1065" && message.contains("refused to start")
            )
        });
        assert!(
            announced,
            "the refusal must reach the notifier, not only the return value: {seen:?}"
        );

        registry.shutdown_all();
    }

    /// Issue #1065, the trigger-independence half: the refusal is a property of
    /// the PUBLISH failing, not of any one filesystem condition.
    ///
    /// `ContextTooLarge` is checked before a filesystem is touched at all — the
    /// composed context is simply bigger than `MAX_CONTEXT_BYTES`, here because
    /// the start role's own `prompt_template` is. So this reaches the same
    /// refusal with no directory, no mode and no symlink involved, which is what
    /// keeps the guarantee from being read as "the 0775 case is handled".
    #[tokio::test]
    async fn a_context_too_large_to_publish_refuses_the_spawn_just_as_a_bad_directory_does() {
        let dir = crate::test_temp::tempdir().expect("tempdir for the orchestration cwd");
        let oversized = "x".repeat(crate::orchestrator_context::MAX_CONTEXT_BYTES + 1);

        let registry = Arc::new(AgentPtyRegistry::new());
        let notifier = RecordingNotifier::default();

        let err = match spawn(
            unpublishable_orchestration_request(dir.path(), Some(oversized)),
            &registry,
            &notifier,
            None,
            false,
            None,
        )
        .await
        {
            Ok(_) => {
                registry.shutdown_all();
                panic!("a context over the size cap must refuse the spawn too");
            }
            Err(e) => e,
        };

        assert!(
            matches!(
                err,
                SpawnError::OrchestratorContext(
                    crate::orchestrator_context::ContextPublishError::ContextTooLarge(_)
                )
            ),
            "expected the size refusal, got: {err:?}"
        );
        assert!(
            registry.agent_records().is_empty(),
            "no role may start for a context that was never publishable"
        );
        assert!(
            !dir.path().join(".dot-agent-deck").exists(),
            "the size cap is checked before a filesystem is touched, so nothing should have \
             been created"
        );

        registry.shutdown_all();
    }

    /// Issue #1065's boundary: a producer that composes NOTHING
    /// (`compose_orchestrator_context: None` — the #120 issue-dispatch and #127
    /// scheduler paths) is untouched by the refusal.
    ///
    /// Those paths deliver `prompt` verbatim and never publish a context, so
    /// there is no precondition for them to fail. Pinned because the obvious
    /// over-reach of this fix — refusing whenever a context file is absent —
    /// would break two shipped features that correctly have no context file at
    /// all. Same occupied `.dot-agent-deck` as the refusal test above, so the
    /// ONLY difference between green here and refused there is the field.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_producer_that_composes_no_context_is_unaffected_by_the_refusal() {
        let dir = crate::test_temp::tempdir().expect("tempdir for the orchestration cwd");
        std::fs::write(dir.path().join(".dot-agent-deck"), b"not a directory")
            .expect("occupy the context dir path");

        let registry = Arc::new(AgentPtyRegistry::new());
        let notifier = RecordingNotifier::default();

        let mut req = unpublishable_orchestration_request(dir.path(), None);
        req.compose_orchestrator_context = None;

        // `detach_delivery: true` is what the #120 issue-dispatch producer passes
        // (no effect since PRD #1666). The assertion is about the spawn being
        // ALLOWED.
        let handle = spawn(req, &registry, &notifier, None, true, None)
            .await
            .expect("a producer that composes no context must still spawn");
        assert_eq!(handle.agents.len(), 2, "both roles must have started");
        assert!(
            notifier.0.lock().expect("notifier mutex").is_empty(),
            "nothing was refused, so nothing should have been announced"
        );

        registry.shutdown_all();
    }

    /// Issue #960: the one producer of a dispatched orchestration's run title,
    /// at its edges. The two ordinary branches are covered behaviourally by
    /// `orchestration/dispatch/005`; what is only reachable here is the
    /// root/empty cwd and the "never `Some(\"\")`" property the fallback depends
    /// on — both the hydration fallback (`tab.rs`) and
    /// `validate_tab_membership` read an empty title as absent, so stamping one
    /// would defeat the fallback to the canonical name rather than replace it.
    #[test]
    fn a_dispatched_run_title_is_either_absent_or_a_real_label() {
        // The ordinary case: the cwd basename is the per-run identity, and
        // `name` leads so the canonical label survives tab-strip truncation.
        assert_eq!(
            dispatched_orchestration_display_title("mixed", "/w/repo-issue-950"),
            Some("mixed · repo-issue-950".to_string())
        );
        // Nothing to add when the basename IS the name (an unnamed orchestration
        // whose name resolved to its own cwd basename).
        assert_eq!(
            dispatched_orchestration_display_title("mixed", "/w/mixed"),
            None
        );
        // No basename to add: a root or empty cwd. `file_name()` is `None` for
        // both, and for a trailing `..` — none of which can name a run.
        for cwd in ["/", "", "/w/.."] {
            assert_eq!(
                dispatched_orchestration_display_title("mixed", cwd),
                None,
                "cwd {cwd:?} has no basename to identify a run by"
            );
        }
        // Whatever it returns, it is never an EMPTY title. Enumerated over the
        // inputs that could plausibly produce one rather than argued from the
        // format string, since the fallback's correctness rests on it.
        for (name, cwd) in [("", "/w/issue-1"), ("mixed", "/w/ "), ("", "/")] {
            assert_ne!(
                dispatched_orchestration_display_title(name, cwd).as_deref(),
                Some(""),
                "an empty title is read as absent everywhere, so it must never be produced \
                 (name = {name:?}, cwd = {cwd:?})"
            );
        }
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

    /// Issue #770: the recognizer must agree with the producer it sits beside,
    /// in BOTH directions — a role id is recognized and a non-role id is not.
    /// Pinned against `next_pane_id`'s real output rather than hand-written
    /// strings, so a change to either one fails here instead of silently
    /// unhooking the orphan detection from the ids it is meant to catch.
    #[test]
    fn next_pane_id_role_ids_are_the_ones_recognized_as_orchestration_roles() {
        for idx in [0usize, 1, 7, 12] {
            let role = next_pane_id("github-issues", Some(idx));
            assert!(
                is_orchestration_role_pane_id(&role),
                "a role id must be recognized: {role}"
            );
        }
        let single = next_pane_id("github-issues", None);
        assert!(
            !is_orchestration_role_pane_id(&single),
            "a non-role scheduled pane id must NOT be recognized: {single}"
        );
        // A task name that already ends in the role shape cannot forge one:
        // the counter is appended after it.
        let forged = next_pane_id("nightly-r0", None);
        assert!(
            !is_orchestration_role_pane_id(&forged),
            "a task name ending in the role shape must not forge a role id: {forged}"
        );
        // Ids from the other minting path (the TUI's numeric allocator) and
        // anything without the daemon's prefix answer `false`.
        for other in ["0", "12", "r0", "orchestrator-r0", "sched-", "sched-x-r"] {
            assert!(
                !is_orchestration_role_pane_id(other),
                "not a daemon-spawned role id: {other}"
            );
        }
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
            live_agent_id: Some("agent-1".into()),
            kind: None,
        });
        assert_eq!(
            decide_reuse(true, existing, None),
            ReuseDecision::SpawnFresh
        );
    }

    #[test]
    fn decide_reuse_reuses_live_tab_by_default() {
        let existing = Some(ExistingTab {
            pane_id: "p1".into(),
            live_agent_id: Some("agent-1".into()),
            kind: None,
        });
        assert_eq!(
            decide_reuse(false, existing, None),
            ReuseDecision::Reuse {
                pane_id: "p1".into(),
                agent_id: "agent-1".into(),
            }
        );
    }

    #[test]
    fn decide_reuse_spawns_fresh_when_no_entry_or_stale() {
        assert_eq!(decide_reuse(false, None, None), ReuseDecision::SpawnFresh);
        let stale = Some(ExistingTab {
            pane_id: "p1".into(),
            live_agent_id: None,
            kind: None,
        });
        assert_eq!(decide_reuse(false, stale, None), ReuseDecision::SpawnFresh);
    }

    // Issue #835 — a fire that explicitly asks for a shape must NOT re-deliver
    // into a live tab of a different shape. This is the upgrade path a user
    // actually takes: the schedule has been firing a team, they set
    // `shape = "single"`, and without this the next fire lands right back in the
    // old orchestrator pane — the very surprise the field exists to remove.
    #[test]
    fn decide_reuse_spawns_fresh_when_the_requested_shape_differs() {
        let team = || {
            Some(ExistingTab {
                pane_id: "p1".into(),
                live_agent_id: Some("agent-1".into()),
                kind: Some(SpawnKind::Orchestration {
                    name: "team".into(),
                }),
            })
        };
        // live orchestration tab, but this fire wants one agent
        assert_eq!(
            decide_reuse(false, team(), Some(&SpawnKind::SingleAgent)),
            ReuseDecision::SpawnFresh
        );
        // live orchestration tab, but this fire wants a DIFFERENT orchestration
        assert_eq!(
            decide_reuse(
                false,
                team(),
                Some(&SpawnKind::Orchestration {
                    name: "other".into()
                })
            ),
            ReuseDecision::SpawnFresh
        );
    }

    // The matching half: an explicit shape that AGREES with the recorded tab
    // reuses it, so declaring a shape does not silently disable tab reuse.
    #[test]
    fn decide_reuse_reuses_when_the_requested_shape_matches() {
        let single = Some(ExistingTab {
            pane_id: "p1".into(),
            live_agent_id: Some("agent-1".into()),
            kind: Some(SpawnKind::SingleAgent),
        });
        assert_eq!(
            decide_reuse(false, single, Some(&SpawnKind::SingleAgent)),
            ReuseDecision::Reuse {
                pane_id: "p1".into(),
                agent_id: "agent-1".into(),
            }
        );
        let team = Some(ExistingTab {
            pane_id: "p2".into(),
            live_agent_id: Some("agent-1".into()),
            kind: Some(SpawnKind::Orchestration {
                name: "team".into(),
            }),
        });
        assert_eq!(
            decide_reuse(
                false,
                team,
                Some(&SpawnKind::Orchestration {
                    name: "team".into()
                })
            ),
            ReuseDecision::Reuse {
                pane_id: "p2".into(),
                agent_id: "agent-1".into(),
            }
        );
    }

    // A task with NO declared shape is untouched: the shape is derived per fire
    // and never compared, so a config-derived task reuses its tab exactly as it
    // did before #835 — even when the recorded kind is known.
    #[test]
    fn decide_reuse_ignores_kind_when_no_shape_was_requested() {
        let team = Some(ExistingTab {
            pane_id: "p1".into(),
            live_agent_id: Some("agent-1".into()),
            kind: Some(SpawnKind::Orchestration {
                name: "team".into(),
            }),
        });
        assert_eq!(
            decide_reuse(false, team, None),
            ReuseDecision::Reuse {
                pane_id: "p1".into(),
                agent_id: "agent-1".into(),
            }
        );
    }

    // `kind_of_target` is the request side of that comparison: it must read the
    // TARGET's shape, including the orchestration's resolved name, so a rename
    // in the config counts as a different shape.
    #[test]
    fn kind_of_target_reads_the_targets_shape() {
        assert_eq!(
            kind_of_target(&SpawnTarget::SingleAgent {
                command: Some("cat".into()),
                inherited: None,
            }),
            SpawnKind::SingleAgent
        );
        let orch = crate::project_config::OrchestrationConfig {
            name: "team".to_string(),
            default: false,
            roles: Vec::new(),
        };
        assert_eq!(
            kind_of_target(&SpawnTarget::Orchestration {
                name: "team".into(),
                roles: Vec::new(),
                config: Box::new(orch),
            }),
            SpawnKind::Orchestration {
                name: "team".into()
            }
        );
    }

    /// Issue #617 (finding 2): a recorded tab whose delivery pane is live but
    /// owned by an agent this task never spawned is NOT reusable. `live_agent_id`
    /// is `None` in exactly that case — the caller resolves it by intersecting
    /// the pane's current agent with `ReuseEntry::agent_ids` — so the decision
    /// falls to `SpawnFresh` rather than re-delivering a scheduled prompt into a
    /// stranger that inherited the pane id.
    #[test]
    fn decide_reuse_spawns_fresh_when_the_pane_id_was_inherited_by_another_agent() {
        let inherited = Some(ExistingTab {
            pane_id: "p1".into(),
            live_agent_id: None,
            // Issue #835's shape gate is not what refuses this one: the kind
            // matches what the fire asks for, so the refusal below is the
            // identity gate and nothing else.
            kind: Some(SpawnKind::SingleAgent),
        });
        assert_eq!(
            decide_reuse(false, inherited, Some(&SpawnKind::SingleAgent)),
            ReuseDecision::SpawnFresh
        );
    }

    // --- Phase 2B deliver-on-idle decision (M2.2 / Q6) ---

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
            None,
            Some("morning-digest"),
            "42",
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
        // Issue #1507: the registry id rides on the metadata for the TUI's
        // creation order, while `agent_id` stays `None` (asserted above).
        assert_eq!(
            e.metadata
                .get(crate::event::SURFACED_AGENT_ID_METADATA_KEY)
                .map(String::as_str),
            Some("42"),
            "the surfaced agent's registry id must ride on the event for ordering"
        );
        // Issue #684: and it declares itself DAEMON-AUTHORED, so an attached
        // TUI's `AppState` does not read it as a conversation announcing itself
        // over this pane. Without the marker it established the pane's hook
        // generation — the pane id, since that is this event's `session_id` — a
        // TUI-owned seed or role prompt bound that as its delivery target, and
        // the agent's genuine `SessionStart` moments later read as a rollover and
        // discarded the prompt. Asserted on the PRODUCER because the behavioural
        // test (`prompt/pane-input/033`) replays this event rather than calling
        // this function, so nothing else would notice the marker going missing.
        assert!(
            e.is_card_surface_session_start(),
            "the card-surfacing start must carry its origin marker; metadata={:?}",
            e.metadata
        );
        assert!(
            e.is_daemon_synthetic(),
            "and must therefore fall in the daemon-authored class, which is what \
             `AppState::apply_event` and the delivery paths discriminate on"
        );
    }

    fn attach_record(
        pane_id: Option<&str>,
        display_name: Option<&str>,
        tab_membership: Option<TabMembership>,
    ) -> crate::agent_pty::AgentRecord {
        crate::agent_pty::AgentRecord {
            id: "7".into(),
            pane_id_env: pane_id.map(str::to_string),
            display_name: display_name.map(str::to_string),
            cwd: Some("/work/role".into()),
            tab_membership,
            agent_type: Some(AgentType::OpenCode),
            rows: 24,
            cols: 80,
            live: None,
            spawned_at_ms: None,
            cli_name: None,
            prompt_keys: None,
            crashed: None,
            orchestrator_context_path: None,
            authoring_kind: None,
            inbox: None,
        }
    }

    fn orchestration_membership(role_name: &str) -> TabMembership {
        TabMembership::Orchestration {
            name: "team".into(),
            role_index: 1,
            role_name: role_name.into(),
            is_start_role: false,
            orchestration_cwd: Some("/work/team".into()),
            display_title: Some("Team run".into()),
            orchestration_id: Some("orch-1".into()),
        }
    }

    fn drain(rx: &mut broadcast::Receiver<BroadcastMsg>) -> Vec<BroadcastMsg> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// PRD #1223: a dashboard pane started over the attach socket is announced
    /// as the card-surfacing `SessionStart`, titled from the record and filed
    /// under the TUI's placeholder key.
    #[test]
    fn attach_started_dashboard_pane_surfaces_one_titled_card() {
        let (tx, mut rx) = broadcast::channel(8);
        let record = attach_record(Some("desktop-ab-0"), Some("my-agent"), None);
        surface_attach_started_agent(&tx, &record, Some("opencode"));
        let msgs = drain(&mut rx);
        let [BroadcastMsg::Event(e)] = msgs.as_slice() else {
            panic!("expected exactly one card event, got {msgs:?}");
        };
        assert!(e.is_card_surface_session_start());
        assert_eq!(
            e.session_id,
            crate::state::placeholder_session_id("desktop-ab-0")
        );
        assert_eq!(e.pane_id.as_deref(), Some("desktop-ab-0"));
        assert_eq!(e.cwd.as_deref(), Some("/work/role"));
        assert_eq!(e.agent_type, AgentType::OpenCode);
        assert_eq!(
            e.metadata
                .get(DISPLAY_NAME_METADATA_KEY)
                .map(String::as_str),
            Some("my-agent")
        );
        assert!(e.agent_id.is_none());
    }

    /// PRD #1223: an unnamed start carries no display-name metadata rather than
    /// an invented one.
    #[test]
    fn attach_started_unnamed_pane_carries_no_display_name() {
        let (tx, mut rx) = broadcast::channel(8);
        surface_attach_started_agent(&tx, &attach_record(Some("p-0"), None, None), None);
        let msgs = drain(&mut rx);
        let [BroadcastMsg::Event(e)] = msgs.as_slice() else {
            panic!("expected exactly one card event, got {msgs:?}");
        };
        assert!(!e.metadata.contains_key(DISPLAY_NAME_METADATA_KEY));
    }

    /// PRD #1223: an orchestration role is announced as a ONE-role surface
    /// keyed on the orchestration's shared identity, THEN its role-named card.
    #[test]
    fn attach_started_orchestration_role_surfaces_tab_then_card() {
        let (tx, mut rx) = broadcast::channel(8);
        let record = attach_record(
            Some("desktop-ab-1"),
            Some("builder"),
            Some(orchestration_membership("builder")),
        );
        surface_attach_started_agent(&tx, &record, None);
        let msgs = drain(&mut rx);
        let [
            BroadcastMsg::OrchestrationSurface(surface),
            BroadcastMsg::Event(e),
        ] = msgs.as_slice()
        else {
            panic!("expected a surface then a card, got {msgs:?}");
        };
        assert_eq!(surface.name, "team");
        assert_eq!(surface.cwd, "/work/team", "the SHARED cwd, not the role's");
        assert_eq!(surface.display_title.as_deref(), Some("Team run"));
        assert_eq!(surface.orchestration_id.as_deref(), Some("orch-1"));
        assert_eq!(surface.roles.len(), 1);
        let role = &surface.roles[0];
        assert_eq!(role.pane_id, "desktop-ab-1");
        assert_eq!(role.role_index, 1);
        assert_eq!(role.role_name, "builder");
        assert!(!role.is_start_role);
        assert_eq!(e.pane_id.as_deref(), Some("desktop-ab-1"));
        assert_eq!(e.cwd.as_deref(), Some("/work/role"));
        assert_eq!(
            e.metadata
                .get(DISPLAY_NAME_METADATA_KEY)
                .map(String::as_str),
            Some("builder")
        );
    }

    /// Issue #1395 item 1: the start role's surface carries the context path
    /// stamped onto its record from daemon state; a non-start role's never
    /// does, even if its record somehow carried one.
    #[test]
    fn attach_started_surface_carries_the_context_path_for_the_start_role_only() {
        let path =
            "/work/team/.dot-agent-deck/orchestrator-context-0123456789abcdef0123456789abcdef.md";
        let (tx, mut rx) = broadcast::channel(8);
        for start in [true, false] {
            let mut membership = orchestration_membership("lead");
            if let TabMembership::Orchestration { is_start_role, .. } = &mut membership {
                *is_start_role = start;
            }
            let mut record = attach_record(Some("desktop-ab-0"), Some("lead"), Some(membership));
            record.orchestrator_context_path = Some(path.into());
            surface_attach_started_agent(&tx, &record, None);
            let msgs = drain(&mut rx);
            let Some(BroadcastMsg::OrchestrationSurface(surface)) = msgs.first() else {
                panic!("expected a surface first, got {msgs:?}");
            };
            assert_eq!(
                surface.context_path.as_deref(),
                start.then_some(path),
                "start role = {start}"
            );
        }
    }

    /// PRD #1223: nothing is announced for a pane with no pane id, a mode pane,
    /// or a role with no role name — none has a live surface to land on.
    #[test]
    fn attach_started_agent_without_a_live_surface_emits_nothing() {
        let (tx, mut rx) = broadcast::channel(8);
        for record in [
            attach_record(None, Some("x"), None),
            attach_record(
                Some("mode-0"),
                None,
                Some(TabMembership::Mode {
                    name: "review".into(),
                }),
            ),
            attach_record(Some("role-0"), None, Some(orchestration_membership(""))),
        ] {
            surface_attach_started_agent(&tx, &record, None);
            assert!(drain(&mut rx).is_empty(), "nothing for {record:?}");
        }
    }

    /// PRD #1223 idempotency: the daemon's card and the sending TUI's own
    /// placeholder are ONE card in an attached TUI whichever arrives first.
    #[test]
    fn attach_surfaced_card_and_tui_placeholder_are_one_card_in_either_order() {
        let (tx, mut rx) = broadcast::channel(8);
        surface_attach_started_agent(&tx, &attach_record(Some("tui-p"), Some("mine"), None), None);
        let msgs = drain(&mut rx);
        let [BroadcastMsg::Event(event)] = msgs.as_slice() else {
            panic!("expected one card event, got {msgs:?}");
        };
        let cards_on_pane = |state: &crate::state::AppState| {
            state
                .sessions
                .values()
                .filter(|s| s.pane_id.as_deref() == Some("tui-p"))
                .count()
        };

        // Daemon broadcast first, then the TUI's own post-start placeholder.
        let mut state = crate::state::AppState::default();
        state.apply_event(event.clone());
        state.register_pane("tui-p".into());
        state.insert_placeholder_session(
            "tui-p".into(),
            Some("/work/role".into()),
            None,
            Some("7".into()),
        );
        assert_eq!(cards_on_pane(&state), 1, "broadcast then placeholder");
        assert_eq!(
            state.sessions[&crate::state::placeholder_session_id("tui-p")]
                .agent_id
                .as_deref(),
            Some("7"),
            "the TUI's placeholder, with its generation, is the card that stays"
        );

        // The TUI's placeholder first, then the daemon broadcast.
        let mut state = crate::state::AppState::default();
        state.register_pane("tui-p".into());
        state.insert_placeholder_session(
            "tui-p".into(),
            Some("/work/role".into()),
            None,
            Some("7".into()),
        );
        state.apply_event(event.clone());
        assert_eq!(cards_on_pane(&state), 1, "placeholder then broadcast");
        assert_eq!(
            state.sessions[&crate::state::placeholder_session_id("tui-p")]
                .agent_id
                .as_deref(),
            Some("7"),
            "an untagged card-surface event must not blank the card's generation"
        );
    }

    /// PRD #1223: a stop is announced as ONE marked `SessionEnd` naming the
    /// stopped agent, filed under the placeholder key the start's card used.
    #[test]
    fn attach_stopped_agent_announces_one_marked_session_end() {
        let (tx, mut rx) = broadcast::channel(8);
        let record = attach_record(
            Some("desktop-ab-1"),
            Some("builder"),
            Some(orchestration_membership("builder")),
        );
        surface_attach_stopped_agent(&tx, &record, "desktop-ab-1");
        let msgs = drain(&mut rx);
        let [BroadcastMsg::Event(e)] = msgs.as_slice() else {
            panic!("expected exactly one event, got {msgs:?}");
        };
        assert!(e.is_daemon_pane_closed());
        assert_eq!(e.event_type, EventType::SessionEnd);
        assert_eq!(
            e.session_id,
            crate::state::placeholder_session_id("desktop-ab-1")
        );
        assert_eq!(e.pane_id.as_deref(), Some("desktop-ab-1"));
        assert_eq!(e.agent_id.as_deref(), Some("7"));
        assert!(e.is_daemon_synthetic());

        // And the TUI side removes the card the matching start drew.
        let (tx, mut rx) = broadcast::channel(8);
        let dashboard = attach_record(Some("desktop-ab-0"), Some("mine"), None);
        surface_attach_started_agent(&tx, &dashboard, None);
        surface_attach_stopped_agent(&tx, &dashboard, "desktop-ab-0");
        let mut state = crate::state::AppState::default();
        for msg in drain(&mut rx) {
            let BroadcastMsg::Event(e) = msg else {
                panic!("expected events only");
            };
            state.apply_event(e);
        }
        assert!(
            state.sessions.is_empty(),
            "start then stop leaves no card: {:?}",
            state.sessions
        );
        assert!(!state.managed_pane_ids.contains("desktop-ab-0"));
    }

    #[test]
    fn surface_spawned_pane_send_is_noop_without_subscribers() {
        // The standalone-daemon case (no attached TUI): `send` errs, swallowed.
        let (tx, rx) = broadcast::channel::<BroadcastMsg>(8);
        drop(rx);
        surface_spawned_pane(&tx, "sched-x-0", "/tmp/x", None, None, Some("x"), "1");
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

    /// Scenario: A scheduled task reuses its tab while the user has just typed
    /// into it, so the fire posts its prompt to the inbox of the agent the reuse
    /// decision resolved rather than typing it. That agent then goes away and an
    /// unrelated one inherits the same `DOT_AGENT_DECK_PANE_ID`. The prompt
    /// becomes undeliverable instead of passing to the successor: the
    /// successor's inbox holds nothing, and none of the prompt's bytes reach
    /// its pane, whose scrollback still shows a later authorized write.
    #[spec("scheduler/reuse/004")]
    #[tokio::test]
    async fn reuse_004_a_hand_over_after_the_post_never_reaches_the_successor() {
        const PANE: &str = "reuse-handover-after-post";
        const TASK: &str = "reuse-handover-task";
        const PROMPT: &str = "SCHEDULED-PROMPT-MUST-NOT-REACH-THE-SUCCESSOR-6a1e";
        const BARRIER: &str = "AUTHORIZED-WRITE-AFTER-THE-HAND-OVER-3c88";

        struct SilentNotifier;
        impl Notifier for SilentNotifier {
            fn notify(&self, _event: NotifyEvent) {}
        }

        let registry = Arc::new(AgentPtyRegistry::new());
        let original = spawn_byte_target(&registry, PANE);
        // A user draft no longer holds the post back: the doorbell's draft
        // guard is what keeps the prompt out of the user's way.
        registry.note_user_input(PANE);
        let reuse: ReuseRegistry = Arc::default();
        reuse.lock().unwrap().insert(
            TASK.to_string(),
            ReuseEntry {
                agent_ids: vec![original.clone()],
                delivery_pane_id: PANE.to_string(),
                kind: SpawnKind::SingleAgent,
            },
        );
        let req = SpawnRequest {
            task_name: TASK.to_string(),
            working_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            command: None,
            prompt: PROMPT.to_string(),
            resolved_target: None,
            compose_orchestrator_context: None,
            seed_reason: None,
        };
        spawn_or_reuse(
            req,
            false,
            &registry,
            &reuse,
            &SilentNotifier,
            Duration::from_millis(600),
            None,
            None,
        )
        .await
        .expect("a reused tab takes the fire");

        let posted = registry.inbox().list(PANE);
        assert_eq!(posted.len(), 1, "one post per fire: {posted:?}");
        let id = posted[0].id.to_string();
        assert_eq!(
            posted[0].bind_agent.as_deref(),
            Some(original.as_str()),
            "the prompt must be bound to the agent the reuse decision resolved"
        );
        assert_eq!(posted[0].state, crate::inbox::StateName::Queued);

        registry
            .close_agent(&original)
            .expect("close the agent the reuse decision resolved");
        let deadline = Instant::now() + Duration::from_secs(10);
        while registry
            .inbox()
            .list(PANE)
            .iter()
            .any(|m| m.id.as_str() == id && m.state != crate::inbox::StateName::Undeliverable)
        {
            assert!(
                Instant::now() < deadline,
                "the bound prompt must become undeliverable once its agent exits: {:?}",
                registry.inbox().list(PANE)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let successor = spawn_byte_target(&registry, PANE);
        assert_ne!(
            original, successor,
            "the hand-over must produce a NEW registry agent id"
        );
        assert!(
            registry
                .inbox()
                .doorbell_facts(PANE, &successor)
                .queued
                .is_empty(),
            "the successor's inbox must not inherit the predecessor's prompt"
        );

        // A barrier, not a sleep: an AUTHORIZED write — the doorbell's, the
        // one automatic writer the deck keeps — that has demonstrably arrived
        // proves the successor's PTY has drained past the point where a leaked
        // prompt would have landed.
        let barrier = registry
            .write_doorbell(PANE, &successor, false, None, Some(BARRIER), || async {
                true
            })
            .await;
        assert_eq!(barrier, crate::agent_pty::DoorbellWrite::Written);
        crate::test_pty_wait::wait_for_echo_bytes(&registry, &successor, BARRIER.as_bytes()).await;
        let snapshot = registry.snapshot(&successor).expect("successor snapshot");
        assert!(
            !String::from_utf8_lossy(&snapshot).contains(PROMPT),
            "a scheduled task's prompt reached a process that merely inherited the pane id of \
             the agent the reuse decision resolved; snapshot = {:?}",
            String::from_utf8_lossy(&snapshot)
        );

        registry.shutdown_all();
    }
}
