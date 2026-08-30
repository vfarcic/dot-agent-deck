mod daemon_bridge;
mod dto;
mod terminal;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use dot_agent_deck::agent_pty::{
    DOT_AGENT_DECK_PANE_ID, TabMembership, is_valid_display_name, mint_orchestration_id,
};
use dot_agent_deck::config::attach_socket_path;
use dot_agent_deck::daemon_client::{DaemonClient, EventSubscription, StartAgentOptions};
use dot_agent_deck::daemon_stop::{StopOutcome, run_daemon_stop};
use dot_agent_deck::event::{AgentType, BroadcastMsg, EventType, SendResult};
use dot_agent_deck::project_config::{OrchestrationConfig, load_project_config};
use dot_agent_deck::prompt_delivery::AUTOMATIC_PROMPT_DEADLINE;

/// Maximum wait for an agent readiness signal before the coordinator seed
/// falls back to direct delivery. The crate constant this mirrored
/// (`ui::SPAWN_TIME_READINESS_TIMEOUT`) was removed by issue #243's
/// signal-based readiness rework; the nearest crate value
/// (`state::SESSION_START_WAIT_TIMEOUT`, 30s, pub(crate)) answers a different
/// question. Kept desktop-local so this client adds no daemon-side surface.
const SPAWN_TIME_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
use dot_agent_deck::ui::{describe_send_result, is_terminal_send_result, send_retry_delay};
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Emitter, Manager, State, Webview};

use crate::daemon_bridge::{bootstrap, get_snapshot, trusted_daemon};
use crate::dto::{
    BootstrapOptions, COMMAND_MAX_BYTES, ConnectionStatus, DesktopAction, DesktopActionResult,
    DesktopSnapshot, TerminalAttachResult, WorkflowRoleInput,
    ensure_desktop_workflow_platform_supported, mint_desktop_pane_id, safe_message,
    validate_agent_id, validate_start_fields, validate_workflow_shape,
};
use crate::terminal::DesktopState;

const WATCH_RETRY_DELAY: Duration = Duration::from_secs(1);
const COORDINATOR_DELIVERY_RPC_TIMEOUT: Duration = Duration::from_secs(2);

/// Post-`SessionStart` wait before injecting the coordinator seed. See the
/// call site in `deliver_coordinator_prompt` for why the TUI's 500 ms is not
/// enough here. Env override in milliseconds, clamped to 0..=30_000.
fn desktop_seed_buffer() -> Duration {
    const DEFAULT: Duration = Duration::from_millis(3_000);
    match std::env::var("DOT_AGENT_DECK_DESKTOP_SEED_BUFFER_MS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(ms) => Duration::from_millis(ms.min(30_000)),
            Err(_) => DEFAULT,
        },
        Err(_) => DEFAULT,
    }
}

/// Minimum spacing between full-snapshot refreshes driven by the daemon event
/// stream.
///
/// Every hook event used to trigger its own `get_snapshot()` — a `ListAgents`
/// round-trip over the daemon socket — plus a full snapshot serialization
/// across the Tauri IPC bridge and a re-render of the whole deck. A busy agent
/// emits hook events continuously, so with several agents running the webview
/// spent its time re-rendering instead of painting terminals.
///
/// Refreshes are coalesced instead: events that arrive inside the window are
/// absorbed by the next refresh, because `get_snapshot()` always reads current
/// state (latest-wins, never a stale replay). This mirrors the single-slot
/// resize coalescing the terminal bridge already uses.
const SNAPSHOT_COALESCE_INTERVAL: Duration = Duration::from_millis(150);

fn order_workflow_roles(
    config: &OrchestrationConfig,
    requested: &[WorkflowRoleInput],
) -> Result<Vec<WorkflowRoleInput>, String> {
    let mut config_names = HashSet::with_capacity(config.roles.len());
    for role in &config.roles {
        if !config_names.insert(role.name.as_str()) {
            return Err(format!(
                "orchestration config contains duplicate role: {}",
                safe_message(&role.name)
            ));
        }
    }
    let mut requested_by_name = requested
        .iter()
        .map(|role| (role.role.as_str(), role))
        .collect::<HashMap<_, _>>();
    if requested_by_name.len() != requested.len() {
        return Err("workflow request contains duplicate role names".into());
    }

    let mut ordered = Vec::with_capacity(config.roles.len());
    for config_role in &config.roles {
        let requested_role = requested_by_name
            .remove(config_role.name.as_str())
            .ok_or_else(|| {
                format!(
                    "workflow is missing configured role: {}",
                    safe_message(&config_role.name)
                )
            })?;
        if requested_role.start != config_role.start {
            return Err(format!(
                "workflow start marker for role {} does not match .dot-agent-deck.toml",
                safe_message(&config_role.name)
            ));
        }
        ordered.push(requested_role.clone());
    }
    if let Some(extra) = requested_by_name.keys().next() {
        return Err(format!(
            "workflow role is not present in the configured orchestration: {}",
            safe_message(extra)
        ));
    }
    Ok(ordered)
}

fn validate_workflow_against_project(
    name: &str,
    cwd: &str,
    requested: &[WorkflowRoleInput],
) -> Result<(OrchestrationConfig, Vec<WorkflowRoleInput>), String> {
    let config = load_project_config(Path::new(cwd))
        .map_err(|error| safe_message(error.to_string()))?
        .ok_or_else(|| {
            format!(
                "{} has no .dot-agent-deck.toml; a live workflow must match a configured orchestration so daemon delegation routing remains exact",
                safe_message(cwd)
            )
        })?;
    let orchestration = config
        .orchestrations
        .iter()
        .find(|orchestration| orchestration.name == name)
        .ok_or_else(|| {
            format!(
                "orchestration '{}' was not found in {}/.dot-agent-deck.toml",
                safe_message(name),
                safe_message(cwd)
            )
        })?;
    let roles = order_workflow_roles(orchestration, requested)?;
    Ok((orchestration.clone(), roles))
}

fn prepare_workflow_launch(
    name: &str,
    cwd: &str,
    task_prompt: &str,
    requested: &[WorkflowRoleInput],
) -> Result<(Vec<WorkflowRoleInput>, String), String> {
    let task_prompt = task_prompt.trim();
    if task_prompt.is_empty() {
        return Err("task prompt must not be empty".into());
    }
    if task_prompt.len() > COMMAND_MAX_BYTES || task_prompt.contains('\0') {
        return Err(format!(
            "task prompt must be at most {COMMAND_MAX_BYTES} bytes and contain no NUL"
        ));
    }
    let (config, roles) = validate_workflow_against_project(name, cwd, requested)?;
    validate_desktop_coordinator(&roles)?;
    // Upstream #222 moved context composition into `orchestrator_context` and
    // gave it the task parameter, so the daemon spawn path and this one compose
    // identically — the manual "## User task" concat this used to do is now
    // the function's own job.
    let seed = dot_agent_deck::orchestrator_context::prepare_orchestrator_prompt(
        &config,
        cwd,
        Some(task_prompt),
    )
    .ok_or_else(|| {
        format!(
            "could not prepare coordinator context at {}/.dot-agent-deck/orchestrator-context.md; workflow was not started",
            safe_message(cwd)
        )
    })?;
    Ok((roles, seed))
}

fn validate_desktop_coordinator(roles: &[WorkflowRoleInput]) -> Result<&WorkflowRoleInput, String> {
    let start_role = roles
        .iter()
        .find(|role| role.start)
        .ok_or_else(|| "validated workflow has no start role".to_string())?;
    if AgentType::from_command(Some(&start_role.command)) == Some(AgentType::Pi) {
        return Err(
            "Pi cannot be the desktop workflow coordinator in this preview because its native seed delivery has no acknowledgement; choose a non-Pi coordinator or launch the orchestration from the TUI"
                .into(),
        );
    }
    Ok(start_role)
}

#[allow(clippy::too_many_arguments)]
fn workflow_start_options(
    name: &str,
    cwd: &str,
    role: &WorkflowRoleInput,
    role_index: usize,
    orchestration_id: &str,
    pane_id: String,
    rows: u16,
    cols: u16,
) -> StartAgentOptions {
    StartAgentOptions {
        command: Some(role.command.clone()),
        cwd: Some(cwd.to_string()),
        display_name: Some(role.role.clone()),
        rows,
        cols,
        env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id)],
        tab_membership: Some(TabMembership::Orchestration {
            name: name.to_string(),
            role_index,
            role_name: role.role.clone(),
            is_start_role: role.start,
            orchestration_cwd: Some(cwd.to_string()),
            display_title: Some(name.to_string()),
            orchestration_id: Some(orchestration_id.to_string()),
        }),
        agent_type: AgentType::from_command(Some(&role.command)),
        // Desktop workflow coordinators always use the acknowledged delivery
        // path below. Pi coordinators are rejected before this builder runs.
        seed: None,
    }
}

#[allow(async_fn_in_trait)]
trait WorkflowDaemon {
    type ReadinessWatch;

    async fn start_workflow_agent(&self, options: StartAgentOptions) -> Result<String, String>;
    async fn stop_workflow_agent(&self, agent_id: &str) -> Result<(), String>;
    async fn reconcile_workflow_agent(
        &self,
        pane_id: &str,
        orchestration_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, String>;
    async fn begin_coordinator_readiness(&self) -> Result<Self::ReadinessWatch, String>;
    async fn wait_for_coordinator_readiness(
        &self,
        watch: &mut Self::ReadinessWatch,
        pane_id: &str,
        agent_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, String>;
    async fn submit_coordinator_prompt(
        &self,
        pane_id: &str,
        prompt: &str,
        expected_agent_id: &str,
        expected_session_id: Option<&str>,
        delivery_id: &str,
        timeout: Duration,
    ) -> Result<SendResult, String>;
    async fn wait(&self, duration: Duration);
    fn now(&self) -> std::time::Instant;
}

impl WorkflowDaemon for DaemonClient {
    type ReadinessWatch = EventSubscription;

    async fn start_workflow_agent(&self, options: StartAgentOptions) -> Result<String, String> {
        self.start_agent(options)
            .await
            .map_err(|error| safe_message(error.to_string()))
    }

    async fn stop_workflow_agent(&self, agent_id: &str) -> Result<(), String> {
        self.stop_agent(agent_id)
            .await
            .map_err(|error| safe_message(error.to_string()))
    }

    async fn reconcile_workflow_agent(
        &self,
        pane_id: &str,
        orchestration_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        let records = match tokio::time::timeout(timeout, self.list_agents()).await {
            Ok(Ok(records)) => records,
            Ok(Err(error)) => return Err(safe_message(error.to_string())),
            Err(_) => return Err("workflow spawn reconciliation RPC timed out".into()),
        };
        Ok(records
            .into_iter()
            .find(|record| {
                record.pane_id_env.as_deref() == Some(pane_id)
                    && matches!(
                        record.tab_membership.as_ref(),
                        Some(TabMembership::Orchestration {
                            orchestration_id: Some(record_orchestration_id),
                            ..
                        }) if record_orchestration_id == orchestration_id
                    )
            })
            .map(|record| record.id))
    }

    async fn begin_coordinator_readiness(&self) -> Result<Self::ReadinessWatch, String> {
        self.subscribe_events()
            .await
            .map_err(|error| safe_message(error.to_string()))
    }

    async fn wait_for_coordinator_readiness(
        &self,
        watch: &mut Self::ReadinessWatch,
        pane_id: &str,
        agent_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        let wait = async {
            loop {
                match watch.next_event().await {
                    Ok(Some(BroadcastMsg::Event(event)))
                        if event.event_type == EventType::SessionStart
                            && event.pane_id.as_deref() == Some(pane_id)
                            && event.agent_id.as_deref() == Some(agent_id) =>
                    {
                        return Ok(Some(event.session_id));
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => return Err("coordinator readiness stream ended".to_string()),
                    Err(error) => return Err(safe_message(error.to_string())),
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn submit_coordinator_prompt(
        &self,
        pane_id: &str,
        prompt: &str,
        expected_agent_id: &str,
        expected_session_id: Option<&str>,
        delivery_id: &str,
        timeout: Duration,
    ) -> Result<SendResult, String> {
        match tokio::time::timeout(
            timeout,
            self.write_and_submit_with_identity(
                pane_id,
                prompt,
                Some(expected_agent_id),
                expected_session_id,
                Some(delivery_id),
            ),
        )
        .await
        {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(safe_message(error.to_string())),
            Err(_) => Err("coordinator prompt delivery RPC timed out".into()),
        }
    }

    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

#[derive(Debug)]
struct WorkflowLaunchResult {
    start_agent_id: String,
    agent_ids: Vec<String>,
}

async fn rollback_workflow_agents<D: WorkflowDaemon + Sync>(
    daemon: &D,
    started: &[String],
) -> String {
    let mut cleanup_errors = Vec::new();
    for agent_id in started.iter().rev() {
        if let Err(error) = daemon.stop_workflow_agent(agent_id).await {
            cleanup_errors.push(format!(
                "{} ({})",
                safe_message(agent_id),
                safe_message(error)
            ));
        }
    }
    if cleanup_errors.is_empty() {
        format!("stopped {} already-started role(s)", started.len())
    } else {
        format!(
            "cleanup could not confirm stop for {} of {} already-started role(s): {}",
            cleanup_errors.len(),
            started.len(),
            cleanup_errors.join(", ")
        )
    }
}

async fn deliver_coordinator_prompt<D: WorkflowDaemon + Sync>(
    daemon: &D,
    readiness: &mut D::ReadinessWatch,
    pane_id: &str,
    agent_id: &str,
    prompt: &str,
    created_at: std::time::Instant,
) -> Result<(), String> {
    let elapsed = daemon.now().saturating_duration_since(created_at);
    let readiness_wait = SPAWN_TIME_READINESS_TIMEOUT.saturating_sub(elapsed);
    let mut last_failure = None;
    let expected_session_id = if readiness_wait.is_zero() {
        None
    } else {
        match daemon
            .wait_for_coordinator_readiness(readiness, pane_id, agent_id, readiness_wait)
            .await
        {
            Ok(Some(session_id)) => {
                // SessionStart precedes reliable submit handling on slower
                // machines. The TUI's 500 ms buffer proved too short for
                // Claude Code's boot on this hardware — the seed reached the
                // PTY (SendResult Applied) but was discarded by the still-
                // rendering TUI, leaving the coordinator idle at an empty
                // prompt. Default to a longer wait, overridable via
                // DOT_AGENT_DECK_DESKTOP_SEED_BUFFER_MS (clamped to 30 s).
                daemon.wait(desktop_seed_buffer()).await;
                Some(session_id)
            }
            Ok(None) => None,
            Err(error) => {
                last_failure = Some(format!("readiness stream failed: {}", safe_message(error)));
                let remaining = SPAWN_TIME_READINESS_TIMEOUT
                    .saturating_sub(daemon.now().saturating_duration_since(created_at));
                if !remaining.is_zero() {
                    daemon.wait(remaining).await;
                }
                None
            }
        }
    };

    let delivery_id = format!("desktop-seed-{pane_id}");
    let mut attempts = 0u32;
    loop {
        let elapsed = daemon.now().saturating_duration_since(created_at);
        if elapsed >= AUTOMATIC_PROMPT_DEADLINE {
            return Err(format!(
                "coordinator context was not delivered before the {}s deadline{}",
                AUTOMATIC_PROMPT_DEADLINE.as_secs(),
                last_failure
                    .as_deref()
                    .map(|failure| format!(": {failure}"))
                    .unwrap_or_default()
            ));
        }
        let remaining = AUTOMATIC_PROMPT_DEADLINE.saturating_sub(elapsed);
        let rpc_timeout = remaining.min(COORDINATOR_DELIVERY_RPC_TIMEOUT);
        match daemon
            .submit_coordinator_prompt(
                pane_id,
                prompt,
                agent_id,
                expected_session_id.as_deref(),
                &delivery_id,
                rpc_timeout,
            )
            .await
        {
            Ok(SendResult::Applied | SendResult::Queued) => return Ok(()),
            Ok(result) if is_terminal_send_result(result) => {
                return Err(format!(
                    "coordinator context delivery was terminal: {}",
                    describe_send_result(result)
                ));
            }
            Ok(result) => {
                last_failure = Some(describe_send_result(result).to_string());
            }
            Err(error) => {
                last_failure = Some(safe_message(error));
            }
        }
        attempts = attempts.saturating_add(1);
        let remaining = AUTOMATIC_PROMPT_DEADLINE
            .saturating_sub(daemon.now().saturating_duration_since(created_at));
        if remaining.is_zero() {
            continue;
        }
        daemon.wait(send_retry_delay(attempts).min(remaining)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn launch_workflow<D: WorkflowDaemon + Sync>(
    daemon: &D,
    name: &str,
    cwd: &str,
    roles: &[WorkflowRoleInput],
    rows: u16,
    cols: u16,
    orchestration_id: &str,
    orchestrator_seed: &str,
) -> Result<WorkflowLaunchResult, String> {
    validate_desktop_coordinator(roles)?;
    let created_at = daemon.now();
    // Subscribe before the first spawn so a fast Claude SessionStart cannot be
    // lost between process creation and readiness observation.
    let mut readiness = daemon.begin_coordinator_readiness().await?;
    let mut started = Vec::with_capacity(roles.len());
    let mut start_target = None;

    for (role_index, role) in roles.iter().enumerate() {
        let pane_id = mint_desktop_pane_id();
        let options = workflow_start_options(
            name,
            cwd,
            role,
            role_index,
            orchestration_id,
            pane_id.clone(),
            rows,
            cols,
        );
        match daemon.start_workflow_agent(options).await {
            Ok(agent_id) => {
                if role.start {
                    start_target = Some((pane_id, agent_id.clone()));
                }
                started.push(agent_id);
            }
            Err(error) => {
                // StartAgent can spawn/register successfully and then lose its
                // response. Reconcile the already-known pane + orchestration
                // identity before rollback so that just-spawned role is not
                // leaked merely because its id never reached this client.
                let reconciliation_note = match daemon
                    .reconcile_workflow_agent(
                        &pane_id,
                        orchestration_id,
                        COORDINATOR_DELIVERY_RPC_TIMEOUT,
                    )
                    .await
                {
                    Ok(Some(agent_id)) => {
                        if !started.contains(&agent_id) {
                            started.push(agent_id);
                        }
                        String::new()
                    }
                    Ok(None) => String::new(),
                    Err(reconciliation_error) => format!(
                        "; cleanup uncertainty: could not reconcile the failed role by pane and orchestration identity: {}",
                        safe_message(reconciliation_error)
                    ),
                };
                let cleanup_status = rollback_workflow_agents(daemon, &started).await;
                return Err(format!(
                    "failed to start workflow role {}: {}; {cleanup_status}{reconciliation_note}",
                    safe_message(&role.role),
                    safe_message(error)
                ));
            }
        }
    }

    let Some((start_pane_id, start_agent_id)) = start_target else {
        let cleanup_status = rollback_workflow_agents(daemon, &started).await;
        return Err(format!(
            "validated workflow did not start a coordinator; {cleanup_status}"
        ));
    };
    if let Err(error) = deliver_coordinator_prompt(
        daemon,
        &mut readiness,
        &start_pane_id,
        &start_agent_id,
        orchestrator_seed,
        created_at,
    )
    .await
    {
        let cleanup_status = rollback_workflow_agents(daemon, &started).await;
        return Err(format!(
            "workflow coordinator context delivery failed: {}; {cleanup_status}",
            safe_message(error)
        ));
    }

    Ok(WorkflowLaunchResult {
        start_agent_id,
        agent_ids: started,
    })
}

fn ensure_main_webview(webview: &Webview) -> Result<(), String> {
    if webview.label() == "main" {
        Ok(())
    } else {
        Err("desktop bridge commands are scoped to the main webview".into())
    }
}

fn emit_snapshot(app: &AppHandle, snapshot: &DesktopSnapshot) {
    let _ = app.emit("desktop://snapshot", snapshot);
}

fn action_result_ok(send_result: Option<&SendResult>) -> bool {
    send_result.is_none_or(|result| matches!(result, SendResult::Applied | SendResult::Queued))
}

fn ensure_explicit_start_connected(
    start_if_missing: bool,
    snapshot: &DesktopSnapshot,
) -> Result<(), String> {
    if !start_if_missing || snapshot.connection.status == ConnectionStatus::Connected {
        return Ok(());
    }
    Err(snapshot
        .connection
        .error
        .clone()
        .unwrap_or_else(|| "the local daemon did not become connected".into()))
}

async fn refresh_and_emit(app: &AppHandle) -> DesktopSnapshot {
    let snapshot = get_snapshot().await;
    emit_snapshot(app, &snapshot);
    snapshot
}

fn ensure_snapshot_watcher(app: &AppHandle, state: &DesktopState) {
    if !state.start_watcher_once() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let daemon = match trusted_daemon().await {
                Ok(daemon) if daemon.require_compatible().is_ok() => daemon,
                _ => {
                    let snapshot = get_snapshot().await;
                    emit_snapshot(&app, &snapshot);
                    tokio::time::sleep(WATCH_RETRY_DELAY).await;
                    continue;
                }
            };
            let mut subscription = match daemon.client.subscribe_events().await {
                Ok(subscription) => subscription,
                Err(_) => {
                    let snapshot = get_snapshot().await;
                    emit_snapshot(&app, &snapshot);
                    tokio::time::sleep(WATCH_RETRY_DELAY).await;
                    continue;
                }
            };
            let mut last_refresh: Option<tokio::time::Instant> = None;
            while let Ok(Some(event)) = subscription.next_event().await {
                let _ = app.emit("desktop://daemon-event", &event);
                if let Some(previous) = last_refresh {
                    let elapsed = previous.elapsed();
                    if elapsed < SNAPSHOT_COALESCE_INTERVAL {
                        tokio::time::sleep(SNAPSHOT_COALESCE_INTERVAL - elapsed).await;
                    }
                }
                let snapshot = get_snapshot().await;
                emit_snapshot(&app, &snapshot);
                last_refresh = Some(tokio::time::Instant::now());
            }
            tokio::time::sleep(WATCH_RETRY_DELAY).await;
        }
    });
}

#[tauri::command]
async fn desktop_get_snapshot(app: AppHandle, webview: Webview) -> Result<DesktopSnapshot, String> {
    ensure_main_webview(&webview)?;
    Ok(refresh_and_emit(&app).await)
}

#[tauri::command]
async fn desktop_bootstrap(
    app: AppHandle,
    webview: Webview,
    state: State<'_, DesktopState>,
    options: Option<BootstrapOptions>,
) -> Result<DesktopSnapshot, String> {
    ensure_main_webview(&webview)?;
    let options = options.unwrap_or_default();
    let snapshot = bootstrap(&options).await;
    emit_snapshot(&app, &snapshot);
    ensure_snapshot_watcher(&app, &state);
    ensure_explicit_start_connected(options.start_if_missing, &snapshot)?;
    Ok(snapshot)
}

#[tauri::command]
async fn desktop_terminal_attach(
    app: AppHandle,
    webview: Webview,
    state: State<'_, DesktopState>,
    agent_id: String,
    on_output: Channel<Response>,
) -> Result<TerminalAttachResult, String> {
    ensure_main_webview(&webview)?;
    terminal::attach(&app, &state, agent_id, on_output).await
}

#[tauri::command]
async fn desktop_terminal_write(
    webview: Webview,
    state: State<'_, DesktopState>,
    session_id: String,
    data: Vec<u8>,
) -> Result<(), String> {
    ensure_main_webview(&webview)?;
    terminal::write(&state, &session_id, &data).await
}

#[tauri::command]
async fn desktop_terminal_resize(
    webview: Webview,
    state: State<'_, DesktopState>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    ensure_main_webview(&webview)?;
    terminal::resize(&state, &session_id, cols, rows).await
}

#[tauri::command]
async fn desktop_terminal_detach(
    webview: Webview,
    state: State<'_, DesktopState>,
    session_id: String,
) -> Result<bool, String> {
    ensure_main_webview(&webview)?;
    terminal::detach(&state, &session_id).await
}

#[tauri::command]
async fn desktop_run_action(
    app: AppHandle,
    webview: Webview,
    state: State<'_, DesktopState>,
    action: DesktopAction,
) -> Result<DesktopActionResult, String> {
    ensure_main_webview(&webview)?;
    let mut result_agent_id = None;
    let mut result_agent_ids = Vec::new();
    let mut result_send: Option<SendResult> = None;
    let mut result_terminal = None;
    let mut result_message = None;

    match action {
        DesktopAction::Refresh => {}
        DesktopAction::Bootstrap { start_if_missing } => {
            let snapshot = bootstrap(&BootstrapOptions { start_if_missing }).await;
            emit_snapshot(&app, &snapshot);
            ensure_snapshot_watcher(&app, &state);
            ensure_explicit_start_connected(start_if_missing, &snapshot)?;
            return Ok(DesktopActionResult {
                ok: snapshot.connection.status == ConnectionStatus::Connected,
                agent_id: None,
                agent_ids: Vec::new(),
                send_result: None,
                terminal: None,
                message: None,
                snapshot,
            });
        }
        DesktopAction::StartAgent {
            command,
            cwd,
            display_name,
            rows,
            cols,
        } => {
            let (rows, cols) = validate_start_fields(
                command.as_deref(),
                cwd.as_deref(),
                display_name.as_deref(),
                rows.unwrap_or(24),
                cols.unwrap_or(80),
            )?;
            let agent_type = AgentType::from_command(command.as_deref());
            let pane_id = mint_desktop_pane_id();
            let daemon = trusted_daemon().await?;
            daemon.require_compatible()?;
            let id = daemon
                .client
                .start_agent(StartAgentOptions {
                    command,
                    cwd,
                    display_name,
                    rows,
                    cols,
                    env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id)],
                    agent_type,
                    ..Default::default()
                })
                .await
                .map_err(|error| safe_message(error.to_string()))?;
            result_agent_id = Some(id);
        }
        DesktopAction::StartWorkflow {
            name,
            cwd,
            task_prompt,
            roles,
            rows,
            cols,
        } => {
            ensure_desktop_workflow_platform_supported(std::env::consts::OS)?;
            let (rows, cols) = validate_workflow_shape(
                &name,
                &cwd,
                &roles,
                rows.unwrap_or(32),
                cols.unwrap_or(120),
            )?;
            // Match the TUI orchestration path: materialize the canonical
            // config's role/delegation context before any role is spawned.
            // The supported non-Pi coordinator uses the readiness-gated,
            // identity-bound retry path in `launch_workflow`; Pi is rejected
            // before this point because its native path has no acknowledgement.
            // Preparing the file first keeps a context-write failure atomic.
            let (roles, orchestrator_seed) =
                prepare_workflow_launch(&name, &cwd, &task_prompt, &roles)?;
            let orchestration_id = mint_orchestration_id();
            let daemon = trusted_daemon().await?;
            daemon.require_compatible()?;
            let launched = launch_workflow(
                &daemon.client,
                &name,
                &cwd,
                &roles,
                rows,
                cols,
                &orchestration_id,
                &orchestrator_seed,
            )
            .await?;
            result_agent_id = Some(launched.start_agent_id);
            result_agent_ids = launched.agent_ids;
            result_message = Some(
                "Workflow started from the configured orchestration. Commands were applied for this launch only; profile/model command write-back is not implemented."
                    .into(),
            );
        }
        DesktopAction::StopAgent { agent_id } => {
            validate_agent_id(&agent_id)?;
            let daemon = trusted_daemon().await?;
            daemon.require_compatible()?;
            daemon
                .client
                .stop_agent(&agent_id)
                .await
                .map_err(|error| safe_message(error.to_string()))?;
            // Preserve a working attachment when stop fails. Once the daemon
            // confirms the stop, remove any registry entry promptly; the
            // stream reader will also observe STREAM_END and is generation-
            // guarded against removing a newer attachment.
            terminal::detach_agent(&state, &agent_id).await;
            result_agent_id = Some(agent_id);
        }
        DesktopAction::StopDaemon { force } => {
            let outcome = run_daemon_stop(&attach_socket_path(), force)
                .await
                .map_err(|error| safe_message(error.to_string()))?;
            terminal::detach_all(&state).await;
            result_message = Some(match outcome {
                StopOutcome::NoDaemonRunning => "No daemon was running.".into(),
                StopOutcome::Stopped { pid } => format!("Daemon stopped gracefully (pid {pid})."),
                StopOutcome::ForceKilled { pid } => format!("Daemon force-killed (pid {pid})."),
            });
        }
        DesktopAction::RestartDaemon => {
            run_daemon_stop(&attach_socket_path(), false)
                .await
                .map_err(|error| safe_message(error.to_string()))?;
            terminal::detach_all(&state).await;
            let snapshot = bootstrap(&BootstrapOptions {
                start_if_missing: true,
            })
            .await;
            emit_snapshot(&app, &snapshot);
            ensure_snapshot_watcher(&app, &state);
            ensure_explicit_start_connected(true, &snapshot)?;
            return Ok(DesktopActionResult {
                ok: true,
                agent_id: None,
                agent_ids: Vec::new(),
                send_result: None,
                terminal: None,
                message: Some("Daemon replaced with the desktop's matching bundled build.".into()),
                snapshot,
            });
        }
        DesktopAction::RenameAgent {
            agent_id,
            display_name,
        } => {
            validate_agent_id(&agent_id)?;
            if !is_valid_display_name(&display_name) {
                return Err(
                    "displayName is invalid, oversized, empty, or contains control characters"
                        .into(),
                );
            }
            let daemon = trusted_daemon().await?;
            daemon.require_compatible()?;
            let existing_cwd = daemon
                .client
                .list_agents()
                .await
                .map_err(|error| safe_message(error.to_string()))?
                .into_iter()
                .find(|record| record.id == agent_id)
                .ok_or_else(|| format!("agent not found: {agent_id}"))?
                .cwd;
            daemon
                .client
                .set_agent_label(&agent_id, Some(display_name), existing_cwd)
                .await
                .map_err(|error| safe_message(error.to_string()))?;
            result_agent_id = Some(agent_id);
        }
        DesktopAction::AttachTerminal {
            agent_id,
            on_output,
        } => {
            let channel: Channel<Response> = on_output.channel_on(webview.clone());
            let attached = terminal::attach(&app, &state, agent_id.clone(), channel).await?;
            result_agent_id = Some(agent_id);
            result_terminal = Some(attached);
        }
        DesktopAction::DetachTerminal { session_id } => {
            terminal::detach(&state, &session_id).await?;
        }
        DesktopAction::SubmitText { agent_id, text } => {
            validate_agent_id(&agent_id)?;
            if text.is_empty() || text.len() > COMMAND_MAX_BYTES || text.contains('\0') {
                return Err(format!(
                    "text must be 1..={COMMAND_MAX_BYTES} bytes and contain no NUL"
                ));
            }
            let daemon = trusted_daemon().await?;
            daemon.require_compatible()?;
            let record = daemon
                .client
                .list_agents()
                .await
                .map_err(|error| safe_message(error.to_string()))?
                .into_iter()
                .find(|record| record.id == agent_id)
                .ok_or_else(|| format!("agent not found: {agent_id}"))?;
            let pane_id = record.pane_id_env.ok_or_else(|| {
                format!("agent {agent_id} has no pane id and cannot accept submitted text")
            })?;
            result_send = Some(
                daemon
                    .client
                    .write_and_submit_with_identity(&pane_id, &text, Some(&agent_id), None, None)
                    .await
                    .map_err(|error| safe_message(error.to_string()))?,
            );
            result_agent_id = Some(agent_id);
        }
    }

    let snapshot = refresh_and_emit(&app).await;
    let action_ok = action_result_ok(result_send.as_ref());
    Ok(DesktopActionResult {
        // Preserve the daemon's honest delivery semantics: a successfully
        // decoded stale/history-only/etc. outcome is still a non-delivery.
        ok: action_ok,
        agent_id: result_agent_id,
        agent_ids: result_agent_ids,
        send_result: result_send,
        terminal: result_terminal,
        message: result_message,
        snapshot,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .manage(DesktopState::default())
        .invoke_handler(tauri::generate_handler![
            desktop_get_snapshot,
            desktop_bootstrap,
            desktop_terminal_attach,
            desktop_terminal_write,
            desktop_terminal_resize,
            desktop_terminal_detach,
            desktop_run_action,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build dot-agent-deck desktop application");
    app.run(|app_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
        ) {
            let state = app_handle.state::<DesktopState>();
            tauri::async_runtime::block_on(terminal::detach_all(&state));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::project_config::OrchestrationRoleConfig;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct PromptSubmission {
        pane_id: String,
        prompt: String,
        expected_agent_id: String,
        expected_session_id: Option<String>,
        delivery_id: String,
    }

    struct FakeWorkflowDaemon {
        now: Mutex<std::time::Instant>,
        started: Mutex<Vec<StartAgentOptions>>,
        start_results: Mutex<VecDeque<Result<String, String>>>,
        stopped: Mutex<Vec<String>>,
        reconciliation_results: Mutex<VecDeque<Result<Option<String>, String>>>,
        reconciliation_requests: Mutex<Vec<(String, String)>>,
        begin_readiness_count: AtomicUsize,
        readiness: Mutex<Option<Result<Option<String>, String>>>,
        readiness_waits: Mutex<Vec<Duration>>,
        submissions: Mutex<Vec<PromptSubmission>>,
        outcomes: Mutex<VecDeque<Result<SendResult, String>>>,
        fallback_outcome: Result<SendResult, String>,
        sleeps: Mutex<Vec<Duration>>,
    }

    impl FakeWorkflowDaemon {
        fn new(
            readiness: Result<Option<&str>, &str>,
            outcomes: impl IntoIterator<Item = Result<SendResult, String>>,
            fallback_outcome: Result<SendResult, String>,
        ) -> Self {
            Self {
                now: Mutex::new(std::time::Instant::now()),
                started: Mutex::new(Vec::new()),
                start_results: Mutex::new(VecDeque::new()),
                stopped: Mutex::new(Vec::new()),
                reconciliation_results: Mutex::new(VecDeque::new()),
                reconciliation_requests: Mutex::new(Vec::new()),
                begin_readiness_count: AtomicUsize::new(0),
                readiness: Mutex::new(Some(
                    readiness
                        .map(|session| session.map(str::to_string))
                        .map_err(str::to_string),
                )),
                readiness_waits: Mutex::new(Vec::new()),
                submissions: Mutex::new(Vec::new()),
                outcomes: Mutex::new(outcomes.into_iter().collect()),
                fallback_outcome,
                sleeps: Mutex::new(Vec::new()),
            }
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.now.lock().unwrap();
            *now = now.checked_add(duration).expect("test clock overflow");
        }
    }

    impl WorkflowDaemon for FakeWorkflowDaemon {
        type ReadinessWatch = ();

        async fn start_workflow_agent(&self, options: StartAgentOptions) -> Result<String, String> {
            let mut started = self.started.lock().unwrap();
            let agent_id = format!("agent-{}", started.len());
            started.push(options);
            self.start_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(agent_id))
        }

        async fn stop_workflow_agent(&self, agent_id: &str) -> Result<(), String> {
            self.stopped.lock().unwrap().push(agent_id.to_string());
            Ok(())
        }

        async fn reconcile_workflow_agent(
            &self,
            pane_id: &str,
            orchestration_id: &str,
            _timeout: Duration,
        ) -> Result<Option<String>, String> {
            self.reconciliation_requests
                .lock()
                .unwrap()
                .push((pane_id.to_string(), orchestration_id.to_string()));
            self.reconciliation_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(None))
        }

        async fn begin_coordinator_readiness(&self) -> Result<Self::ReadinessWatch, String> {
            self.begin_readiness_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait_for_coordinator_readiness(
            &self,
            _watch: &mut Self::ReadinessWatch,
            _pane_id: &str,
            _agent_id: &str,
            timeout: Duration,
        ) -> Result<Option<String>, String> {
            self.readiness_waits.lock().unwrap().push(timeout);
            let result = self.readiness.lock().unwrap().take().unwrap_or(Ok(None));
            if matches!(&result, Ok(None)) {
                self.advance(timeout);
            }
            result
        }

        async fn submit_coordinator_prompt(
            &self,
            pane_id: &str,
            prompt: &str,
            expected_agent_id: &str,
            expected_session_id: Option<&str>,
            delivery_id: &str,
            _timeout: Duration,
        ) -> Result<SendResult, String> {
            self.submissions.lock().unwrap().push(PromptSubmission {
                pane_id: pane_id.to_string(),
                prompt: prompt.to_string(),
                expected_agent_id: expected_agent_id.to_string(),
                expected_session_id: expected_session_id.map(str::to_string),
                delivery_id: delivery_id.to_string(),
            });
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.fallback_outcome.clone())
        }

        async fn wait(&self, duration: Duration) {
            self.sleeps.lock().unwrap().push(duration);
            self.advance(duration);
        }

        fn now(&self) -> std::time::Instant {
            *self.now.lock().unwrap()
        }
    }

    fn config_role(name: &str, start: bool) -> OrchestrationRoleConfig {
        OrchestrationRoleConfig {
            agent: None,
            name: name.into(),
            command: "configured-command".into(),
            start,
            description: None,
            prompt_template: None,
            clear: true,
        }
    }

    #[test]
    fn workflow_roles_follow_config_order_but_keep_launch_commands() {
        let config = OrchestrationConfig {
            default: false,
            name: "loop".into(),
            roles: vec![config_role("planner", true), config_role("builder", false)],
        };
        let requested = vec![
            WorkflowRoleInput {
                role: "builder".into(),
                command: "codex --model gpt-5.6-sol".into(),
                start: false,
            },
            WorkflowRoleInput {
                role: "planner".into(),
                command: "claude".into(),
                start: true,
            },
        ];
        let ordered = order_workflow_roles(&config, &requested).unwrap();
        assert_eq!(ordered[0].role, "planner");
        assert_eq!(ordered[0].command, "claude");
        assert_eq!(ordered[1].role, "builder");
        assert_eq!(ordered[1].command, "codex --model gpt-5.6-sol");
    }

    #[test]
    fn workflow_roles_reject_missing_or_mismatched_start_role() {
        let config = OrchestrationConfig {
            default: false,
            name: "loop".into(),
            roles: vec![config_role("planner", true), config_role("builder", false)],
        };
        let missing = vec![WorkflowRoleInput {
            role: "planner".into(),
            command: "claude".into(),
            start: true,
        }];
        assert!(order_workflow_roles(&config, &missing).is_err());

        let wrong_start = vec![
            WorkflowRoleInput {
                role: "planner".into(),
                command: "claude".into(),
                start: false,
            },
            WorkflowRoleInput {
                role: "builder".into(),
                command: "codex".into(),
                start: true,
            },
        ];
        assert!(order_workflow_roles(&config, &wrong_start).is_err());
    }

    #[test]
    fn workflow_launch_prepares_canonical_context_in_config_order() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let project_dir = std::env::temp_dir().join(format!(
            "dot-agent-deck-desktop-workflow-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&project_dir).unwrap();
        std::fs::write(
            project_dir.join(".dot-agent-deck.toml"),
            r#"
[[orchestrations]]
name = "loop"

[[orchestrations.roles]]
name = "planner"
command = "configured-planner"
start = true
prompt_template = "Coordinate through the configured team."

[[orchestrations.roles]]
name = "builder"
command = "configured-builder"
description = "Implements the requested change"
"#,
        )
        .unwrap();

        let requested = vec![
            WorkflowRoleInput {
                role: "builder".into(),
                command: "codex --model gpt-5.6-sol".into(),
                start: false,
            },
            WorkflowRoleInput {
                role: "planner".into(),
                command: "claude --model opus".into(),
                start: true,
            },
        ];
        let cwd = project_dir.to_str().unwrap();
        let (roles, seed) =
            prepare_workflow_launch("loop", cwd, "Build the requested feature.", &requested)
                .unwrap();

        assert_eq!(roles[0].role, "planner");
        assert_eq!(roles[0].command, "claude --model opus");
        assert_eq!(roles[1].role, "builder");
        assert_eq!(roles[1].command, "codex --model gpt-5.6-sol");
        assert!(seed.contains(".dot-agent-deck/orchestrator-context.md"));
        // Upstream #222: the seed is now a short pointer and the task itself
        // lands in the context FILE under "## Your task" — asserted below.
        assert!(seed.contains("## Your task"));

        let context =
            std::fs::read_to_string(project_dir.join(".dot-agent-deck/orchestrator-context.md"))
                .unwrap();
        assert!(context.contains("Coordinate through the configured team."));
        assert!(context.contains("**builder**: Implements the requested change"));
        assert!(context.contains("## Delegation protocol"));
        assert!(context.contains("## Your task"));
        assert!(context.contains("Build the requested feature."));

        std::fs::remove_dir_all(project_dir).unwrap();
    }

    fn launch_roles(start_command: &str) -> Vec<WorkflowRoleInput> {
        vec![
            WorkflowRoleInput {
                role: "planner".into(),
                command: start_command.into(),
                start: true,
            },
            WorkflowRoleInput {
                role: "builder".into(),
                command: "codex".into(),
                start: false,
            },
        ]
    }

    #[tokio::test]
    async fn non_pi_launch_waits_for_readiness_and_retries_with_one_identity() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            [Ok(SendResult::NoLiveTarget), Ok(SendResult::Applied)],
            Ok(SendResult::Applied),
        );
        let seed = "Read .dot-agent-deck/orchestrator-context.md and wait.";

        let launched = launch_workflow(
            &daemon,
            "loop",
            "/tmp/project",
            &launch_roles("claude"),
            32,
            120,
            "orchestration-1",
            seed,
        )
        .await
        .unwrap();

        assert_eq!(launched.start_agent_id, "agent-0");
        assert_eq!(
            launched.agent_ids,
            ["agent-0".to_string(), "agent-1".to_string()]
        );
        assert_eq!(daemon.begin_readiness_count.load(Ordering::SeqCst), 1);
        let started = daemon.started.lock().unwrap();
        assert_eq!(started.len(), 2);
        assert_eq!(started[0].seed, None, "Claude must not arm Pi's fallback");
        assert_eq!(started[1].seed, None);
        drop(started);

        let submissions = daemon.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 2);
        assert_eq!(submissions[0].pane_id, submissions[1].pane_id);
        assert_eq!(submissions[0].prompt, seed);
        assert_eq!(submissions[0].expected_agent_id, "agent-0");
        assert_eq!(
            submissions[0].expected_session_id.as_deref(),
            Some("session-planner")
        );
        assert_eq!(submissions[0].delivery_id, submissions[1].delivery_id);
        drop(submissions);

        assert_eq!(
            *daemon.sleeps.lock().unwrap(),
            [desktop_seed_buffer(), send_retry_delay(1)]
        );
        assert!(daemon.stopped.lock().unwrap().is_empty());
    }

    #[test]
    fn desktop_coordinator_guard_rejects_pi_launch_forms() {
        let error = validate_desktop_coordinator(&launch_roles("pi")).unwrap_err();
        assert!(error.contains("Pi cannot be the desktop workflow coordinator"));
        let wrapped_error = validate_desktop_coordinator(&launch_roles("sh -c 'pi'")).unwrap_err();
        assert!(wrapped_error.contains("native seed delivery has no acknowledgement"));
        assert!(validate_desktop_coordinator(&launch_roles("claude")).is_ok());
    }

    #[tokio::test]
    async fn pi_coordinator_is_rejected_before_subscription_or_spawn() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );

        let error = launch_workflow(
            &daemon,
            "loop",
            "/tmp/project",
            &launch_roles("pi"),
            32,
            120,
            "orchestration-1",
            "coordinator prompt",
        )
        .await
        .unwrap_err();

        assert!(error.contains("Pi cannot be the desktop workflow coordinator"));
        assert_eq!(daemon.begin_readiness_count.load(Ordering::SeqCst), 0);
        assert!(daemon.started.lock().unwrap().is_empty());
        assert!(daemon.reconciliation_requests.lock().unwrap().is_empty());
        assert!(daemon.submissions.lock().unwrap().is_empty());
        assert!(daemon.sleeps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hookless_coordinator_uses_timeout_fallback_without_session_guard() {
        let daemon =
            FakeWorkflowDaemon::new(Ok(None), [Ok(SendResult::Applied)], Ok(SendResult::Applied));

        launch_workflow(
            &daemon,
            "loop",
            "/tmp/project",
            &launch_roles("opencode"),
            32,
            120,
            "orchestration-1",
            "coordinator prompt",
        )
        .await
        .unwrap();

        assert_eq!(
            *daemon.readiness_waits.lock().unwrap(),
            [SPAWN_TIME_READINESS_TIMEOUT]
        );
        let submissions = daemon.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].expected_session_id, None);
        assert!(daemon.sleeps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delivery_deadline_rolls_back_every_started_role_in_reverse_order() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            std::iter::empty(),
            Ok(SendResult::NoLiveTarget),
        );

        let error = launch_workflow(
            &daemon,
            "loop",
            "/tmp/project",
            &launch_roles("claude"),
            32,
            120,
            "orchestration-1",
            "coordinator prompt",
        )
        .await
        .unwrap_err();

        assert!(error.contains("60s deadline"), "unexpected error: {error}");
        assert!(error.contains("stopped 2 already-started role(s)"));
        assert_eq!(
            *daemon.stopped.lock().unwrap(),
            ["agent-1".to_string(), "agent-0".to_string()]
        );
        assert!(daemon.submissions.lock().unwrap().len() > 1);
    }

    #[tokio::test]
    async fn lost_start_response_reconciles_failed_pane_before_rollback() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );
        daemon.start_results.lock().unwrap().extend([
            Ok("agent-builder".to_string()),
            Err("start response lost".to_string()),
        ]);
        daemon
            .reconciliation_results
            .lock()
            .unwrap()
            .push_back(Ok(Some("agent-planner".to_string())));
        let roles = vec![
            WorkflowRoleInput {
                role: "builder".into(),
                command: "codex".into(),
                start: false,
            },
            WorkflowRoleInput {
                role: "planner".into(),
                command: "claude".into(),
                start: true,
            },
        ];

        let error = launch_workflow(
            &daemon,
            "loop",
            "/tmp/project",
            &roles,
            32,
            120,
            "orchestration-1",
            "coordinator prompt",
        )
        .await
        .unwrap_err();

        assert!(error.contains("start response lost"));
        assert!(error.contains("stopped 2 already-started role(s)"));
        assert!(!error.contains("cleanup uncertainty"));
        assert_eq!(
            *daemon.stopped.lock().unwrap(),
            ["agent-planner".to_string(), "agent-builder".to_string()]
        );
        let started = daemon.started.lock().unwrap();
        let failed_pane_id = started[1]
            .env
            .iter()
            .find(|(key, _)| key == DOT_AGENT_DECK_PANE_ID)
            .map(|(_, value)| value.clone())
            .unwrap();
        drop(started);
        assert_eq!(
            *daemon.reconciliation_requests.lock().unwrap(),
            [(failed_pane_id, "orchestration-1".to_string())]
        );
    }

    #[tokio::test]
    async fn failed_start_reconciliation_reports_cleanup_uncertainty() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );
        daemon
            .start_results
            .lock()
            .unwrap()
            .push_back(Err("start response lost".to_string()));
        daemon
            .reconciliation_results
            .lock()
            .unwrap()
            .push_back(Err("list-agents unavailable".to_string()));

        let error = launch_workflow(
            &daemon,
            "loop",
            "/tmp/project",
            &launch_roles("claude"),
            32,
            120,
            "orchestration-1",
            "coordinator prompt",
        )
        .await
        .unwrap_err();

        assert!(error.contains("cleanup uncertainty"));
        assert!(error.contains("list-agents unavailable"));
        assert!(error.contains("stopped 0 already-started role(s)"));
        assert!(daemon.stopped.lock().unwrap().is_empty());
        assert_eq!(daemon.reconciliation_requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn action_result_only_marks_delivered_text_as_ok() {
        assert!(action_result_ok(None));
        for delivered in [SendResult::Applied, SendResult::Queued] {
            assert!(action_result_ok(Some(&delivered)));
        }
        for not_delivered in [
            SendResult::Stale,
            SendResult::WrongSession,
            SendResult::HistoryOnly,
            SendResult::NoLiveTarget,
            SendResult::Ambiguous,
            SendResult::Unknown,
        ] {
            assert!(!action_result_ok(Some(&not_delivered)));
        }
    }

    #[test]
    fn explicit_daemon_start_requires_a_connected_snapshot() {
        let disconnected = crate::dto::disconnected_snapshot("daemon start timed out");
        assert!(ensure_explicit_start_connected(false, &disconnected).is_ok());
        assert_eq!(
            ensure_explicit_start_connected(true, &disconnected).unwrap_err(),
            "daemon start timed out"
        );

        let mut connected = disconnected;
        connected.connection.status = ConnectionStatus::Connected;
        connected.connection.error = None;
        assert!(ensure_explicit_start_connected(true, &connected).is_ok());
    }
}
