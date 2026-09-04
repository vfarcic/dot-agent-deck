mod daemon_bridge;
mod dto;
mod terminal;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use dot_agent_deck::agent_pty::{
    DOT_AGENT_DECK_PANE_ID, TabMembership, is_valid_display_name, mint_orchestration_id,
};
use dot_agent_deck::config::attach_socket_path;
use dot_agent_deck::daemon_client::{DaemonClient, EventSubscription, StartAgentOptions};
use dot_agent_deck::daemon_stop::{StopOutcome, run_daemon_stop};
use dot_agent_deck::event::{
    AgentType, BroadcastMsg, EventType, PreparedWorkflow, ProjectRole, SendResult,
};
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

use crate::daemon_bridge::{
    allow_build_mismatch_this_session, bootstrap, get_snapshot, trusted_daemon,
};
use crate::dto::{
    BootstrapOptions, COMMAND_MAX_BYTES, ConnectionStatus, DesktopAction, DesktopActionResult,
    DesktopProjectListing, DesktopResolvedProject, DesktopSnapshot, TerminalAttachResult,
    WorkflowRoleInput, ensure_desktop_workflow_platform_supported, map_project_listing,
    map_resolved_project, mint_desktop_pane_id, safe_message, validate_agent_id,
    validate_pasted_project_path, validate_start_fields, validate_workflow_shape,
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

/// Put the launch's per-role commands into the ORCHESTRATION's role order, and
/// refuse any disagreement about the role set.
///
/// PRD #819 M6: `configured` is now the daemon's projected role list
/// ([`dot_agent_deck::event::ProjectRole`], off `prepare-workflow`) rather than
/// an `OrchestrationConfig` this process read off its own filesystem. The rule
/// is unchanged — same names, same order, same start marker — but the authority
/// for it moved to the machine the agents will actually run on.
fn order_workflow_roles(
    configured: &[ProjectRole],
    requested: &[WorkflowRoleInput],
) -> Result<Vec<WorkflowRoleInput>, String> {
    let mut config_names = HashSet::with_capacity(configured.len());
    for role in configured {
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

    let mut ordered = Vec::with_capacity(configured.len());
    for config_role in configured {
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
                "workflow start marker for role {} does not match the orchestration the daemon prepared",
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

/// PRD #819 M6: the launch preparation, performed **by the daemon**.
///
/// What this replaced was `validate_workflow_against_project` plus
/// `prepare_workflow_launch`: a `load_project_config` against the desktop's own
/// filesystem, and a `prepare_orchestrator_prompt` that created a directory and
/// wrote a file there. Against a remote daemon both read and wrote the WRONG
/// machine, and neither errored — the launch validated against a config no agent
/// would ever see and published the coordinator context where no agent would
/// ever read it. That is the defect PRD #819 exists to remove.
///
/// **The ordering inverts here, deliberately.** The comment this displaced said
/// "preparing the file first keeps a context-write failure atomic", and that
/// claim was only ever available because the write was local. The daemon is the
/// party that can actually make resolve-and-publish atomic, so it does: one
/// validated config snapshot, resolve, compose, publish, then answer. A
/// preparation that fails answers with a refusal and starts nothing, because
/// nothing here starts anything — the spawn is the caller's next step.
///
/// Returns the roles in the orchestration's own order alongside the daemon's
/// preparation, whose `path` is the canonical spelling the spawn must use and
/// whose `prompt` is the one-liner the coordinator receives.
async fn prepare_workflow_launch<D: WorkflowDaemon + Sync>(
    daemon: &D,
    name: &str,
    cwd: &str,
    task_prompt: &str,
    requested: &[WorkflowRoleInput],
    config_revision: Option<&str>,
) -> Result<(Vec<WorkflowRoleInput>, PreparedWorkflow), String> {
    let task_prompt = task_prompt.trim();
    if task_prompt.is_empty() {
        return Err("task prompt must not be empty".into());
    }
    // A UI affordance, not the bound: the daemon applies its own
    // `bounded_read::MAX_TASK_BYTES` before it touches a filesystem, and it is
    // not entitled to trust this one.
    if task_prompt.len() > COMMAND_MAX_BYTES || task_prompt.contains('\0') {
        return Err(format!(
            "task prompt must be at most {COMMAND_MAX_BYTES} bytes and contain no NUL"
        ));
    }
    let prepared = daemon
        .prepare_workflow(cwd, name, task_prompt, config_revision)
        .await?;
    // Both are `#[serde(default)]` response fields, so an absent one decodes to
    // the empty string rather than failing to parse. Empty means "this daemon
    // did not report it", and neither is something this client may invent: the
    // path would reintroduce the spelling bug the canonical answer exists to
    // close, and the prompt names a project-state file only the daemon wrote.
    if prepared.path.is_empty() {
        return Err(
            "the daemon prepared the workflow but reported no canonical project path; refusing to spawn against an unconfirmed directory"
                .into(),
        );
    }
    if prepared.prompt.trim().is_empty() {
        return Err(
            "the daemon prepared the workflow but reported no coordinator prompt; the context would never be read"
                .into(),
        );
    }
    let roles = order_workflow_roles(&prepared.roles, requested)?;
    validate_desktop_coordinator(&roles)?;
    Ok((roles, prepared))
}

/// PRD #819 audit fix: turn a **withheld** `prepare-workflow` into the outcome
/// it actually is, rather than letting the launch fail on
/// `DaemonCapabilities::require`'s uniform withhold sentence.
///
/// The daemon strikes `prepare-workflow` from `DAEMON_CAPABILITIES` where the
/// publish cannot deliver the owner-only guarantee it documents — the mode
/// bits, the `O_NOFOLLOW | O_DIRECTORY` open and the group/other-write refusal
/// are all Unix, and the constant is `#[cfg(not(unix))]`-narrowed to the two
/// read-only verbs. It also refuses the verb outright there, with
/// `unsupported-platform`; but a client that gates on the capability never
/// reaches that refusal, so without this the *only* thing the user would see is
/// a sentence about capability advertisement.
///
/// The inference is narrow on purpose, and each clause is load-bearing:
///
/// * the caller has already passed `require_compatible`, so this daemon speaks
///   **this exact** `PROTOCOL_VERSION` — an older daemon, which advertises
///   nothing, is not what is being classified here;
/// * `is_advertised()` excludes the unadvertised case, which stays with the
///   generic sentence because "we know nothing about this daemon's verbs" is a
///   different fact;
/// * `list-projects` must be present, because that is what a platform-narrowed
///   set looks like. A daemon advertising some other set entirely is not making
///   a statement about its platform, and gets the generic sentence too.
///
/// It carries the daemon's own `unsupported-platform` code so the webview has
/// one thing to recognise for one outcome, whichever side concluded it.
fn ensure_daemon_can_prepare(
    capabilities: Option<&dot_agent_deck::daemon_client::DaemonCapabilities>,
) -> Result<(), String> {
    use dot_agent_deck::daemon_protocol::{
        CAP_LIST_PROJECTS, CAP_PREPARE_WORKFLOW, PROJECT_ERR_UNSUPPORTED_PLATFORM,
    };

    let Some(capabilities) = capabilities else {
        return Ok(());
    };
    if !capabilities.is_advertised()
        || capabilities.supports(CAP_PREPARE_WORKFLOW)
        || !capabilities.supports(CAP_LIST_PROJECTS)
    {
        return Ok(());
    }
    Err(format!(
        "{PROJECT_ERR_UNSUPPORTED_PLATFORM}: this daemon offers the project verbs but withholds \
         `{CAP_PREPARE_WORKFLOW}`, which is what a daemon does when its platform cannot give the \
         published coordinator context an owner-only guarantee. Nothing was started. Launch this \
         workflow from the TUI on the daemon's own host, or point the app at a Unix daemon."
    ))
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

    /// PRD #819 M6: ask the daemon to resolve the project, compose the
    /// coordinator context and publish it. The only step of a launch that
    /// writes, and it happens on the daemon's filesystem rather than this one's.
    async fn prepare_workflow(
        &self,
        cwd: &str,
        orchestration: &str,
        task: &str,
        config_revision: Option<&str>,
    ) -> Result<PreparedWorkflow, String>;

    /// PRD #819 audit fix (P2, finding 2): confirm the daemon behind this
    /// endpoint still enforces preparation tokens, on a handshake taken
    /// immediately before a prepared spawn.
    ///
    /// Called once per prepared role rather than once per launch, because each
    /// role's spawn is its own connection and therefore its own exposure. See
    /// `daemon_bridge::verify_prepared_launch_peer` for what it closes and for
    /// the sliver it does not.
    async fn verify_prepared_launch_peer(&self) -> Result<(), String>;

    /// `prep_token` is the one the preparation handed back, presented so a
    /// spawn against a preparation that has since gone stale is refused
    /// daemon-side. It is a staleness check and not an authorization — see
    /// `dot_agent_deck::prep_token`'s module doc.
    async fn start_workflow_agent(
        &self,
        options: StartAgentOptions,
        prep_token: Option<&str>,
    ) -> Result<String, String>;
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

    async fn prepare_workflow(
        &self,
        cwd: &str,
        orchestration: &str,
        task: &str,
        config_revision: Option<&str>,
    ) -> Result<PreparedWorkflow, String> {
        DaemonClient::prepare_workflow(self, cwd, orchestration, task, config_revision)
            .await
            .map_err(|error| safe_message(error.to_string()))
    }

    async fn verify_prepared_launch_peer(&self) -> Result<(), String> {
        daemon_bridge::verify_prepared_launch_peer(self.socket_path()).await
    }

    async fn start_workflow_agent(
        &self,
        options: StartAgentOptions,
        prep_token: Option<&str>,
    ) -> Result<String, String> {
        self.start_agent_with_prep_token(options, prep_token)
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
    prep_token: Option<&str>,
) -> Result<WorkflowLaunchResult, String> {
    validate_desktop_coordinator(roles)?;
    let created_at = daemon.now();
    // Subscribe before the first spawn so a fast Claude SessionStart cannot be
    // lost between process creation and readiness observation.
    let mut readiness = daemon.begin_coordinator_readiness().await?;
    let mut started = Vec::with_capacity(roles.len());
    let mut start_target = None;

    for (role_index, role) in roles.iter().enumerate() {
        // PRD #819 audit fix (P2, finding 2): this spawn presents a preparation
        // token, and it is about to open its OWN connection to do so — one that
        // exchanges no `Hello` and re-checks no protocol version. An older
        // daemon substituted since the preparation would accept the stable
        // `StartAgent` shape, ignore the unknown `prep_token` key, and start the
        // role with no preparation enforcement at all. So re-confirm the peer
        // per role, and refuse rather than spawn if it cannot be confirmed:
        // "we could not verify" is not "it is fine".
        //
        // Only for a prepared spawn. A token-less start has no preparation to
        // protect and is left byte-for-byte as it was.
        if prep_token.is_some()
            && let Err(error) = daemon.verify_prepared_launch_peer().await
        {
            let cleanup_status = rollback_workflow_agents(daemon, &started).await;
            return Err(format!(
                "refusing to start workflow role {} against a daemon that no longer confirms it \
                 enforces this preparation: {}; {cleanup_status}",
                safe_message(&role.role),
                safe_message(error)
            ));
        }
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
        match daemon.start_workflow_agent(options, prep_token).await {
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

/// PRD #819 M6: the projects THIS DAEMON knows about.
///
/// Every path the desktop offers comes from here or from the user; none is
/// derived from the desktop's own environment, which is the whole invariant.
/// An empty listing is a successful answer — "this daemon has nothing live and
/// its startup cwd is not a project" — and the webview renders its
/// paste-a-path surface for it rather than an error.
#[tauri::command]
async fn desktop_list_projects(webview: Webview) -> Result<DesktopProjectListing, String> {
    ensure_main_webview(&webview)?;
    let daemon = trusted_daemon().await?;
    daemon.require_compatible()?;
    let listing = daemon
        .client
        .list_projects()
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    Ok(map_project_listing(listing))
}

/// PRD #819 M6: resolve ONE path — a path this daemon listed, or one the user
/// typed. Read-only, and never a walk.
///
/// The reply's `path` is the daemon's canonical spelling and is what the
/// webview holds from here on: it is the string that goes back on the launch.
/// The string-shape check in front of it touches no filesystem and could not —
/// whether a directory is a project is the daemon's answer, on the daemon's
/// host.
#[tauri::command]
async fn desktop_resolve_project(
    webview: Webview,
    path: String,
) -> Result<DesktopResolvedProject, String> {
    ensure_main_webview(&webview)?;
    validate_pasted_project_path(&path)?;
    let daemon = trusted_daemon().await?;
    daemon.require_compatible()?;
    let project = daemon
        .client
        .resolve_project(&path)
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    Ok(map_resolved_project(project))
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
            config_revision,
        } => {
            ensure_desktop_workflow_platform_supported(std::env::consts::OS)?;
            let (rows, cols) = validate_workflow_shape(
                &name,
                &cwd,
                &roles,
                rows.unwrap_or(32),
                cols.unwrap_or(120),
            )?;
            // PRD #819 M6: the connection comes FIRST now. Resolution used to
            // run two lines above the first daemon contact, against this
            // process's own filesystem; it now runs on the daemon's, so a
            // connection has to exist before a launch can be prepared at all.
            // The supported non-Pi coordinator still uses the readiness-gated,
            // identity-bound retry path in `launch_workflow`; Pi is rejected
            // inside the preparation, before anything is spawned.
            let daemon = trusted_daemon().await?;
            daemon.require_compatible()?;
            ensure_daemon_can_prepare(daemon.client.cached_capabilities().as_ref())?;
            let (roles, prepared) = prepare_workflow_launch(
                &daemon.client,
                &name,
                &cwd,
                &task_prompt,
                &roles,
                config_revision.as_deref(),
            )
            .await?;
            let orchestration_id = mint_orchestration_id();
            let launched = launch_workflow(
                &daemon.client,
                &name,
                // The daemon's CANONICAL spelling, not the one that was sent.
                // An alias or a symlink resolves elsewhere, canonicalising
                // changes the basename, and an empty orchestration name is
                // derived from that basename — so preparing under one spelling
                // and spawning under another is PRD #220's bug verbatim.
                &prepared.path,
                &roles,
                rows,
                cols,
                &orchestration_id,
                &prepared.prompt,
                Some(&prepared.token),
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
        DesktopAction::AllowBuildMismatch => {
            // Session-scoped and nothing else: no daemon call, no persistence,
            // no restart. The refusal it lifts is the desktop's own stamp
            // comparison, so the whole act is setting a process flag and
            // classifying the handshake again — which the `refresh_and_emit`
            // at the tail of this function does unconditionally, and which is
            // why nothing here may cache a verdict.
            allow_build_mismatch_this_session();
            result_message = Some(
                "Build-stamp mismatch accepted for this session; the caveat stays in the connection banner."
                    .into(),
            );
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
            desktop_list_projects,
            desktop_resolve_project,
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

    /// One `prepare-workflow` request, recorded verbatim. PRD #819 M6's whole
    /// point is that the client ASKS, so what it asked is the assertion.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct PrepareRequest {
        cwd: String,
        orchestration: String,
        task: String,
        config_revision: Option<String>,
    }

    struct FakeWorkflowDaemon {
        now: Mutex<std::time::Instant>,
        prepare_requests: Mutex<Vec<PrepareRequest>>,
        prepare_results: Mutex<VecDeque<Result<PreparedWorkflow, String>>>,
        /// PRD #819 audit fix (P2, finding 2): every peer re-verification and
        /// every prepared spawn, in the order they happened. Ordering is the
        /// property under test — a verification that runs AFTER the spawn it
        /// guards guards nothing — so one log rather than two counters.
        peer_log: Mutex<Vec<String>>,
        peer_results: Mutex<VecDeque<Result<(), String>>>,
        start_tokens: Mutex<Vec<Option<String>>>,
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
                prepare_requests: Mutex::new(Vec::new()),
                prepare_results: Mutex::new(VecDeque::new()),
                peer_log: Mutex::new(Vec::new()),
                peer_results: Mutex::new(VecDeque::new()),
                start_tokens: Mutex::new(Vec::new()),
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

        async fn prepare_workflow(
            &self,
            cwd: &str,
            orchestration: &str,
            task: &str,
            config_revision: Option<&str>,
        ) -> Result<PreparedWorkflow, String> {
            self.prepare_requests.lock().unwrap().push(PrepareRequest {
                cwd: cwd.to_string(),
                orchestration: orchestration.to_string(),
                task: task.to_string(),
                config_revision: config_revision.map(str::to_string),
            });
            self.prepare_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(prepared_workflow()))
        }

        async fn verify_prepared_launch_peer(&self) -> Result<(), String> {
            self.peer_log.lock().unwrap().push("verify".into());
            self.peer_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(()))
        }

        async fn start_workflow_agent(
            &self,
            options: StartAgentOptions,
            prep_token: Option<&str>,
        ) -> Result<String, String> {
            self.peer_log.lock().unwrap().push(format!(
                "start:{}",
                options.display_name.as_deref().unwrap_or("?")
            ));
            self.start_tokens
                .lock()
                .unwrap()
                .push(prep_token.map(str::to_string));
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

    fn config_role(name: &str, start: bool) -> ProjectRole {
        ProjectRole {
            name: name.into(),
            start,
        }
    }

    /// The daemon's default answer to `prepare-workflow`: a canonical path that
    /// deliberately DIFFERS from every spelling the tests send, so a spawn that
    /// reuses the caller's string instead of the daemon's is a failed assertion
    /// rather than a coincidence.
    fn prepared_workflow() -> PreparedWorkflow {
        PreparedWorkflow {
            context_path: "/canonical/project/.dot-agent-deck/orchestrator-context.md".into(),
            path: "/canonical/project".into(),
            token: "prep-token-1".into(),
            roles: vec![config_role("planner", true), config_role("builder", false)],
            prompt: "Read .dot-agent-deck/orchestrator-context.md and carry out your task.".into(),
        }
    }

    #[test]
    fn workflow_roles_follow_config_order_but_keep_launch_commands() {
        let config = [config_role("planner", true), config_role("builder", false)];
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
        let config = [config_role("planner", true), config_role("builder", false)];
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

    /// PRD #819 M6 rewrote this test rather than deleting it, because the
    /// migration destroys its premise rather than its subject.
    ///
    /// What it used to do: write a real `.dot-agent-deck.toml` into a temp
    /// directory, call the old `prepare_workflow_launch`, and read the
    /// coordinator context back **off this machine's disk**. Every one of those
    /// steps is now a defect — the client neither reads a project config nor
    /// writes a context, and against a remote daemon doing either was silently
    /// wrong. The content half of that assertion was re-established daemon-side
    /// by the lane-1 `project/launch/001`, so it is not lost; what belongs here
    /// is the half only this seam can state.
    ///
    /// So it now pins that the client **asks**. The fixture keeps the real
    /// config file, and keeps it saying something DIFFERENT from what the fake
    /// daemon answers, precisely so a reintroduced local read fails the test
    /// instead of passing it by agreeing with itself.
    #[tokio::test]
    async fn workflow_launch_asks_the_daemon_and_reads_no_project_from_disk() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let project_dir = std::env::temp_dir().join(format!(
            "dot-agent-deck-desktop-workflow-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&project_dir).unwrap();
        // A config that DISAGREES with the daemon: reversed role order, and a
        // start marker on the other role. A client that reads it would order
        // `builder` first and reject the requested start marker.
        std::fs::write(
            project_dir.join(".dot-agent-deck.toml"),
            r#"
[[orchestrations]]
name = "loop"

[[orchestrations.roles]]
name = "builder"
command = "configured-builder"
start = true

[[orchestrations.roles]]
name = "planner"
command = "configured-planner"
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
        let cwd = project_dir.to_str().unwrap().to_string();
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );

        let (roles, prepared) = prepare_workflow_launch(
            &daemon,
            "loop",
            &cwd,
            "  Build the requested feature.  ",
            &requested,
            Some("revision-7"),
        )
        .await
        .unwrap();

        // The request went out verbatim — including the trimmed task and the
        // revision the picker resolved against, which is what closes the window
        // between the pick and the write.
        assert_eq!(
            *daemon.prepare_requests.lock().unwrap(),
            [PrepareRequest {
                cwd: cwd.clone(),
                orchestration: "loop".into(),
                task: "Build the requested feature.".into(),
                config_revision: Some("revision-7".into()),
            }]
        );

        // The DAEMON's order won, not the file's. Launch commands are still the
        // client's own, which is the one thing this side still owns.
        assert_eq!(roles[0].role, "planner");
        assert_eq!(roles[0].command, "claude --model opus");
        assert_eq!(roles[1].role, "builder");
        assert_eq!(roles[1].command, "codex --model gpt-5.6-sol");
        // The coordinator prompt is the daemon's, not a sentence composed here.
        assert_eq!(prepared.prompt, prepared_workflow().prompt);
        assert_eq!(prepared.token, "prep-token-1");

        // And nothing was written to this machine. The old test read this exact
        // file back and asserted on its contents; now its absence is the point.
        assert!(
            !project_dir.join(".dot-agent-deck").exists(),
            "the client must publish no coordinator context of its own"
        );

        std::fs::remove_dir_all(project_dir).unwrap();
    }

    /// The invariant, at the seam where PRD #220's bug would recur: the
    /// canonical spelling the daemon prepared against is the one every spawn
    /// uses, not the spelling the caller sent.
    #[tokio::test]
    async fn the_prepared_canonical_path_is_the_spawn_cwd() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            [Ok(SendResult::Applied)],
            Ok(SendResult::Applied),
        );
        let (roles, prepared) = prepare_workflow_launch(
            &daemon,
            "loop",
            // A symlinked alias, whose basename differs from the canonical
            // directory's — which is exactly what makes an empty orchestration
            // name resolve differently on the two spellings.
            "/home/dev/current",
            "Build it.",
            &launch_roles("claude"),
            None,
        )
        .await
        .unwrap();

        launch_workflow(
            &daemon,
            "loop",
            &prepared.path,
            &roles,
            32,
            120,
            "orchestration-1",
            &prepared.prompt,
            Some(&prepared.token),
        )
        .await
        .unwrap();

        let started = daemon.started.lock().unwrap();
        assert_eq!(started.len(), 2);
        for options in started.iter() {
            assert_eq!(options.cwd.as_deref(), Some("/canonical/project"));
            assert!(
                matches!(
                    options.tab_membership.as_ref(),
                    Some(TabMembership::Orchestration {
                        orchestration_cwd: Some(cwd),
                        ..
                    }) if cwd == "/canonical/project"
                ),
                "the orchestration cwd must be the canonical one too"
            );
        }
        drop(started);

        // Every role presents the preparation's token, so a spawn against a
        // preparation that has since aged out is refused daemon-side.
        assert_eq!(
            *daemon.start_tokens.lock().unwrap(),
            [
                Some("prep-token-1".to_string()),
                Some("prep-token-1".to_string())
            ]
        );
    }

    /// PRD #819 audit fix (P2, finding 2). Scenario: prepare a workflow, then
    /// launch it. Every role that presents the preparation token must
    /// re-confirm the peer FIRST, on its own handshake — because the spawn
    /// itself opens a connection that exchanges no `Hello` and re-checks no
    /// protocol version, so an older daemon substituted in between would ignore
    /// the unknown `prep_token` key and start the role unenforced.
    ///
    /// The assertion is the interleaving, not a count: a verification that runs
    /// after the spawn it guards guards nothing.
    #[tokio::test]
    async fn every_prepared_role_start_re_verifies_the_peer_first() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            [Ok(SendResult::Applied)],
            Ok(SendResult::Applied),
        );
        let (roles, prepared) = prepare_workflow_launch(
            &daemon,
            "loop",
            "/home/dev/project",
            "Build it.",
            &launch_roles("claude"),
            None,
        )
        .await
        .unwrap();

        launch_workflow(
            &daemon,
            "loop",
            &prepared.path,
            &roles,
            32,
            120,
            "orchestration-1",
            &prepared.prompt,
            Some(&prepared.token),
        )
        .await
        .unwrap();

        assert_eq!(
            *daemon.peer_log.lock().unwrap(),
            ["verify", "start:planner", "verify", "start:builder"],
            "each prepared spawn must be preceded by its own peer verification"
        );
    }

    /// The counterpart, and the one that makes the failure CLOSED: a peer that
    /// stops confirming it enforces the preparation refuses the spawn rather
    /// than taking it, and the roles already started are rolled back.
    ///
    /// The refusal lands on the SECOND role deliberately. A launch that dies on
    /// role one proves only that the check is reachable; dying on role two
    /// proves the check is re-made per connection, which is the whole reason it
    /// is inside the loop.
    #[tokio::test]
    async fn a_peer_that_stops_confirming_the_preparation_refuses_and_rolls_back() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            [Ok(SendResult::Applied)],
            Ok(SendResult::Applied),
        );
        {
            let mut peer_results = daemon.peer_results.lock().unwrap();
            peer_results.push_back(Ok(()));
            peer_results.push_back(Err(
                "daemon does not advertise the `prepare-workflow` capability".into(),
            ));
        }
        let (roles, prepared) = prepare_workflow_launch(
            &daemon,
            "loop",
            "/home/dev/project",
            "Build it.",
            &launch_roles("claude"),
            None,
        )
        .await
        .unwrap();

        let error = launch_workflow(
            &daemon,
            "loop",
            &prepared.path,
            &roles,
            32,
            120,
            "orchestration-1",
            &prepared.prompt,
            Some(&prepared.token),
        )
        .await
        .unwrap_err();

        assert!(
            error.contains("no longer confirms it enforces this preparation"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("prepare-workflow"),
            "the peer's own reason must survive: {error}"
        );
        // The second role was never spawned, and the first was stopped.
        assert_eq!(daemon.started.lock().unwrap().len(), 1);
        assert_eq!(*daemon.stopped.lock().unwrap(), ["agent-0".to_string()]);
        assert!(daemon.submissions.lock().unwrap().is_empty());
    }

    /// A token-LESS launch is left exactly as it was: there is no preparation to
    /// protect, so no handshake is spent on one. This is the half that keeps the
    /// fix from becoming a per-spawn tax on every other path.
    #[tokio::test]
    async fn a_token_less_launch_verifies_no_peer() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            [Ok(SendResult::Applied)],
            Ok(SendResult::Applied),
        );

        launch_workflow(
            &daemon,
            "loop",
            "/canonical/project",
            &launch_roles("claude"),
            32,
            120,
            "orchestration-1",
            "seed",
            None,
        )
        .await
        .unwrap();

        assert!(
            !daemon
                .peer_log
                .lock()
                .unwrap()
                .iter()
                .any(|entry| entry == "verify"),
            "a token-less start must not pay for a verification it does not need"
        );
    }

    /// PRD #819 audit fix. Scenario: connect to a daemon that speaks this exact
    /// protocol version, offers the read-only project verbs and withholds
    /// `prepare-workflow`. The launch must stop with the daemon's own
    /// `unsupported-platform` code and a sentence a user can act on, instead of
    /// the uniform capability-withhold sentence that says nothing about why.
    ///
    /// The three negative cases are asserted in the same test because each is a
    /// clause of the inference: an unadvertised daemon, a daemon that DOES
    /// advertise the verb, and a set that is not a platform-narrowed one all
    /// keep the generic path.
    #[test]
    fn a_withheld_prepare_workflow_reads_as_an_unsupported_platform() {
        use dot_agent_deck::daemon_client::DaemonCapabilities;
        use dot_agent_deck::daemon_protocol::{
            AttachResponse, CAP_LIST_PROJECTS, CAP_PREPARE_WORKFLOW, CAP_RESOLVE_PROJECT,
            PROJECT_ERR_UNSUPPORTED_PLATFORM,
        };

        fn advertising(capabilities: &[&str]) -> DaemonCapabilities {
            DaemonCapabilities::from_hello(&AttachResponse {
                capabilities: Some(capabilities.iter().map(|c| (*c).to_string()).collect()),
                ..AttachResponse::ok()
            })
        }

        // The platform-narrowed set: exactly what a non-Unix daemon advertises.
        let error = ensure_daemon_can_prepare(Some(&advertising(&[
            CAP_LIST_PROJECTS,
            CAP_RESOLVE_PROJECT,
        ])))
        .unwrap_err();
        assert!(
            error.starts_with(&format!("{PROJECT_ERR_UNSUPPORTED_PLATFORM}: ")),
            "the code must be the first token so the webview can match it: {error}"
        );
        assert!(
            error.contains("owner-only") && error.contains("Nothing was started"),
            "the sentence must say what happened and why: {error}"
        );

        // Not classified: nothing captured, nothing advertised, the verb present,
        // and a set that says nothing about a platform.
        assert!(ensure_daemon_can_prepare(None).is_ok());
        assert!(ensure_daemon_can_prepare(Some(&DaemonCapabilities::absent())).is_ok());
        assert!(
            ensure_daemon_can_prepare(Some(&advertising(&[
                CAP_LIST_PROJECTS,
                CAP_RESOLVE_PROJECT,
                CAP_PREPARE_WORKFLOW,
            ])))
            .is_ok()
        );
        assert!(ensure_daemon_can_prepare(Some(&advertising(&["something-else"]))).is_ok());
    }

    /// A refused preparation starts nothing — not even a subscription. The
    /// "project left the known set between listing and launch" case arrives
    /// here as exactly this, and the webview presents it like the empty state
    /// rather than as an error.
    #[tokio::test]
    async fn a_refused_preparation_starts_no_roles() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );
        daemon.prepare_results.lock().unwrap().push_back(Err(
            "unresolved: that path is not a project this daemon can offer".into(),
        ));

        let error = prepare_workflow_launch(
            &daemon,
            "loop",
            "/home/dev/gone",
            "Build it.",
            &launch_roles("claude"),
            None,
        )
        .await
        .unwrap_err();

        assert!(error.contains("unresolved"), "unexpected error: {error}");
        assert!(daemon.started.lock().unwrap().is_empty());
        assert_eq!(daemon.begin_readiness_count.load(Ordering::SeqCst), 0);
    }

    /// A daemon that prepared but reported no canonical path, or no coordinator
    /// prompt, is refused rather than papered over. Both fields are
    /// `#[serde(default)]`, so an older daemon decodes them as empty strings —
    /// and neither is something this client may invent: the path would
    /// reintroduce the spelling bug, and the prompt names a project-state file
    /// only the daemon wrote.
    #[tokio::test]
    async fn an_unreported_path_or_prompt_refuses_the_launch() {
        for (mutate, expected) in [
            (
                Box::new(|prepared: &mut PreparedWorkflow| prepared.path.clear())
                    as Box<dyn Fn(&mut PreparedWorkflow)>,
                "canonical project path",
            ),
            (
                Box::new(|prepared: &mut PreparedWorkflow| prepared.prompt = "   ".into()),
                "coordinator prompt",
            ),
        ] {
            let daemon = FakeWorkflowDaemon::new(
                Ok(Some("unused-session")),
                std::iter::empty(),
                Ok(SendResult::Applied),
            );
            let mut prepared = prepared_workflow();
            mutate(&mut prepared);
            daemon
                .prepare_results
                .lock()
                .unwrap()
                .push_back(Ok(prepared));

            let error = prepare_workflow_launch(
                &daemon,
                "loop",
                "/home/dev/project",
                "Build it.",
                &launch_roles("claude"),
                None,
            )
            .await
            .unwrap_err();

            assert!(error.contains(expected), "unexpected error: {error}");
            assert!(daemon.started.lock().unwrap().is_empty());
        }
    }

    /// A Pi coordinator is refused during preparation, before any role is
    /// spawned — the guard moved with the rest of the flow and did not get lost
    /// on the way.
    #[tokio::test]
    async fn a_pi_coordinator_is_refused_during_preparation() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );

        let error = prepare_workflow_launch(
            &daemon,
            "loop",
            "/home/dev/project",
            "Build it.",
            &launch_roles("pi"),
            None,
        )
        .await
        .unwrap_err();

        assert!(error.contains("Pi cannot be the desktop workflow coordinator"));
        assert!(daemon.started.lock().unwrap().is_empty());
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
            Some("prep-token-1"),
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
            Some("prep-token-1"),
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
            Some("prep-token-1"),
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
            Some("prep-token-1"),
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
            Some("prep-token-1"),
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
            Some("prep-token-1"),
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
