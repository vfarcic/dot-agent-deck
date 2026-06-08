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
//!    delivers the prompt through the UNGATED
//!    [`AgentPtyRegistry::write_to_pane_and_submit`] (payload + CR, routed by
//!    `DOT_AGENT_DECK_PANE_ID`) after a short buffer delay — NOT gated on a
//!    SessionStart "agent-ready" hook, so bare commands (a shell, `cat`) still
//!    receive the prompt.
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
use crate::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
use crate::project_config::{ProjectConfig, load_project_config, resolve_orchestration_name};
use crate::scheduler::{Notifier, NotifyEvent};

/// Buffer delay between spawning the PTY and writing the prompt, so the child
/// and the registry's pump reader are wired before bytes flow. Deliberately a
/// fixed delay, NOT a SessionStart gate (bare commands have no hook).
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
    Orchestration { name: String, roles: Vec<RoleSpawn> },
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
        let roles = orch
            .roles
            .iter()
            .enumerate()
            .map(|(i, r)| RoleSpawn {
                role_index: i,
                role_name: r.name.clone(),
                command: r.command.clone(),
                is_start_role: r.start,
            })
            .collect();
        return SpawnTarget::Orchestration { name, roles };
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
pub async fn spawn(
    req: SpawnRequest,
    registry: &AgentPtyRegistry,
    notifier: &dyn Notifier,
    event_tx: Option<&broadcast::Sender<BroadcastMsg>>,
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

    // 2. Branch on the target dir's config.
    let config = load_config_for_dir(dir);
    let target = decide_target(config.as_ref(), dir, req.command.as_deref());

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
            // — no new broadcast variant. Orchestration fires are intentionally
            // NOT surfaced this way: a proper orchestration tab is rebuilt by
            // the TUI's hydration/partition path, which a flat SessionStart
            // can't reconstruct, and live multi-orchestration surfacing is the
            // #140 session-partitioning concern.
            if let Some(tx) = event_tx {
                surface_spawned_pane(tx, &pane_id, &req.working_dir, command.as_deref());
            }
            deliver(registry, &pane_id, &req.prompt).await;
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
        SpawnTarget::Orchestration { name, roles } => {
            let orch_idx = orchestrator_role_index(&roles);
            let mut agents = Vec::with_capacity(roles.len());
            for role in &roles {
                let pane_id = next_pane_id(&req.task_name, Some(role.role_index));
                let membership = TabMembership::Orchestration {
                    name: name.clone(),
                    role_index: role.role_index,
                    role_name: role.role_name.clone(),
                    is_start_role: role.is_start_role,
                    orchestration_cwd: Some(req.working_dir.clone()),
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
            // Deliver the prompt to the orchestrator role pane.
            let delivery_pane_id = agents[orch_idx].pane_id.clone();
            deliver(registry, &delivery_pane_id, &req.prompt).await;
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
fn pane_env(pane_id: &str, pin_sh: bool) -> Vec<(String, String)> {
    let mut env = vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())];
    if pin_sh {
        env.push(("SHELL".to_string(), "/bin/sh".to_string()));
    }
    env
}

/// Deliver the prompt into the pane via the ungated write-and-submit path after
/// a short buffer delay. Delivery failure is logged, not fatal — the tab is
/// already open.
async fn deliver(registry: &AgentPtyRegistry, pane_id: &str, prompt: &str) {
    tokio::time::sleep(DELIVER_BUFFER_DELAY).await;
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
fn surface_spawned_pane(
    event_tx: &broadcast::Sender<BroadcastMsg>,
    pane_id: &str,
    cwd: &str,
    command: Option<&str>,
) {
    let event = AgentEvent {
        session_id: pane_id.to_string(),
        agent_type: AgentType::from_command(command).unwrap_or(AgentType::None),
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: Some(cwd.to_string()),
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: HashMap::new(),
        pane_id: Some(pane_id.to_string()),
        agent_id: None,
    };
    let _ = event_tx.send(BroadcastMsg::Event(event));
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
    registry: &AgentPtyRegistry,
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
            let handle = spawn(req, registry, notifier, event_tx).await?;
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
            SpawnTarget::Orchestration { name, roles } => {
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

        // multi-word (pin_sh=true) → pane-id + the SHELL wrapper override.
        let env = pane_env("sched-x-1", true);
        assert!(env.iter().any(|(k, v)| k == "SHELL" && v == "/bin/sh"));
    }

    // finding #2 — the synthetic SessionStart surfaced to attached TUIs is a
    // SessionStart for the spawned pane, rooted at the spawn cwd, with
    // `agent_id == None` so a later real hook supersedes (not duplicates) it.
    #[test]
    fn surface_spawned_pane_emits_session_start_for_attached_tuis() {
        let (tx, mut rx) = broadcast::channel(8);
        surface_spawned_pane(
            &tx,
            "sched-livecard-0",
            "/tmp/scratch/livecard",
            Some("cat"),
        );
        let BroadcastMsg::Event(e) = rx.try_recv().expect("a broadcast must be queued");
        assert_eq!(e.event_type, EventType::SessionStart);
        assert_eq!(e.pane_id.as_deref(), Some("sched-livecard-0"));
        assert_eq!(e.cwd.as_deref(), Some("/tmp/scratch/livecard"));
        assert!(
            e.agent_id.is_none(),
            "agent_id must be None so a real SessionStart hook supersedes the placeholder"
        );
    }

    #[test]
    fn surface_spawned_pane_send_is_noop_without_subscribers() {
        // The standalone-daemon case (no attached TUI): `send` errs, swallowed.
        let (tx, rx) = broadcast::channel::<BroadcastMsg>(8);
        drop(rx);
        surface_spawned_pane(&tx, "sched-x-0", "/tmp/x", None);
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
