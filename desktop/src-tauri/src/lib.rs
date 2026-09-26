mod agent_view;
mod appearance;
mod daemon_bridge;
mod dto;
mod endpoint_test;
// Tests only: the shared endpoint-validation table, read from here and from
// `desktop/src/lib/endpoints.test.ts`. No item outside `#[cfg(test)]`, so it
// adds nothing to a normal build.
#[cfg(test)]
mod endpoint_field_parity;
mod endpoint_tunnels;
mod generation;
// PRD #802 — the two validating newtypes the `[voice]` stages store their
// service coordinates in. A module of its own rather than one under `voice`
// for `secrets`' reason: `ALLOWED_FIELD_TYPES` refuses a `String` in any
// settings struct, so the next feature that needs to store an endpoint uses
// these rather than inventing a second answer.
mod model_service;
// PRD #802 M4 — the credential seam PRD #803 M5 named, over the OS keychain.
// Not voice-specific, so it is a module of its own rather than one under
// `voice`: the rule it serves is #803's and any later feature needing a
// credential uses the same store.
mod secrets;
// Tests only: the counted sweep that keeps `dto::DeckScope` from decaying into
// a convention (issue #1116). No item outside `#[cfg(test)]`, so it adds
// nothing to a normal build.
#[cfg(test)]
mod selection_capture;
mod settings;
mod terminal;
// PRD #802 — the voice command table, its Rust-side consumers, the microphone
// and the transcription seam.
//
// `pub` rather than private because most of it still has no in-crate consumer:
// M6 owns the surface. M7 gave part of it one — the four `desktop_voice_*`
// commands below are the IPC seam the panel will drive.
pub mod voice;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use dot_agent_deck::agent_pty::{
    DOT_AGENT_DECK_PANE_ID, TabMembership, is_valid_display_name, mint_orchestration_id,
};
use dot_agent_deck::authoring_seeds::AuthoringKind;
use dot_agent_deck::daemon_client::{
    ClientError, DaemonClient, Endpoint, EventSubscription, GatedQuery, StartAgentOptions,
};
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

use crate::agent_view::{AgentView, RECONCILE_INTERVAL};
use crate::daemon_bridge::{
    DaemonLinks, allow_build_mismatch_this_session, bootstrap, get_snapshot, snapshot_with,
    trusted_daemon,
};
use crate::dto::{
    BootstrapOptions, COMMAND_MAX_BYTES, ConnectionStatus, DesktopAction, DesktopActionError,
    DesktopActionResult, DesktopAgentOption, DesktopDirectoryListing, DesktopNewAgentOptions,
    DesktopNewAgentOrchestrations, DesktopProjectListing, DesktopResolvedProject, DesktopSnapshot,
    TerminalAttachResult, WorkflowRoleInput, desktop_agent_registry,
    ensure_desktop_workflow_platform_supported, map_project_listing, map_resolved_project,
    mint_desktop_pane_id, safe_message, selected_endpoint, validate_agent_id, validate_dimensions,
    validate_pasted_project_path, validate_start_fields, validate_workflow_shape,
};
use crate::secrets::{
    KeychainSecretStore, Secret, SecretError, SecretId, SecretStatus, SecretStore,
};
use crate::settings::DesktopSettings;
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
                "workflow start marker for role {} does not match the orchestration the deck prepared",
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
            "the deck prepared the workflow but reported no canonical project path; refusing to spawn against an unconfirmed directory"
                .into(),
        );
    }
    if prepared.prompt.trim().is_empty() {
        return Err(
            "the deck prepared the workflow but reported no coordinator prompt; the context would never be read"
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
        "{PROJECT_ERR_UNSUPPORTED_PLATFORM}: this deck offers the project verbs but withholds \
         `{CAP_PREPARE_WORKFLOW}`, which is what a deck does when its platform cannot give the \
         published coordinator context an owner-only guarantee. Nothing was started. Launch this \
         workflow from the TUI on that deck's own host, or point the app at a deck on a Unix host."
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

    /// `prep_token` is the one the preparation handed back. A token routes the
    /// spawn onto `start-prepared-agent`, where the token is a required field,
    /// so a daemon that does not know that verb refuses the request outright and
    /// the launch fails closed with nothing started. It is a staleness check and
    /// not an authorization — see `dot_agent_deck::prep_token`'s module doc.
    async fn start_workflow_agent(
        &self,
        options: StartAgentOptions,
        prep_token: Option<&str>,
    ) -> Result<String, RoleStartFailure>;
    /// PRD #1223 M6: start one prepared role with the command its project
    /// config gives it, on the deck — `DaemonClient::start_prepared_role`,
    /// which withholds (`Unsupported`, nothing sent) from a deck that does not
    /// advertise `prepared-role-command`.
    async fn start_configured_role(
        &self,
        options: StartAgentOptions,
        prep_token: &str,
    ) -> Result<GatedQuery<String>, RoleStartFailure>;
    /// The agent type the deck recorded for `agent_id` at spawn — for a
    /// configured role, the role's resolved type. `None` when it recorded none.
    async fn launched_agent_type(&self, agent_id: &str) -> Result<Option<AgentType>, String>;
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
    /// Issue #1028: the cancel-safe end of a drained
    /// [`EventSubscription`], not the subscription itself. See
    /// [`Self::begin_coordinator_readiness`].
    type ReadinessWatch = tokio::sync::mpsc::Receiver<BroadcastMsg>;

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

    async fn start_workflow_agent(
        &self,
        options: StartAgentOptions,
        prep_token: Option<&str>,
    ) -> Result<String, RoleStartFailure> {
        self.start_agent_with_prep_token(options, prep_token)
            .await
            .map_err(RoleStartFailure::from_client)
    }

    async fn start_configured_role(
        &self,
        options: StartAgentOptions,
        prep_token: &str,
    ) -> Result<GatedQuery<String>, RoleStartFailure> {
        self.start_prepared_role(options, prep_token)
            .await
            .map_err(RoleStartFailure::from_client)
    }

    async fn launched_agent_type(&self, agent_id: &str) -> Result<Option<AgentType>, String> {
        let records = crate::daemon_bridge::bounded_reply(
            "ListAgents for the coordinator's agent type",
            self.list_agents(),
        )
        .await?;
        records
            .into_iter()
            .find(|record| record.id == agent_id)
            .map(|record| record.agent_type)
            .ok_or_else(|| "the deck no longer lists the coordinator it just started".to_string())
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
        // Issue #1084: bounded, like the fleet path's own `SubscribeEvents`
        // and the handshake before it. A deck that is down fails at once with
        // `ECONNREFUSED`, but one that takes the connection and never answers
        // left this await with no deadline at all — a launch parked for as long
        // as the peer held the socket open, with nothing on screen saying why.
        //
        // Only the RESP that CONFIRMS the subscription is bounded, and the
        // ordering is what keeps that true: this completes BEFORE the draining
        // task below exists, so the single future this timeout can drop is
        // `subscribe_events()`'s own round trip. It is never `next_event`, whose
        // cancel-unsafety issue #1028 is about, and never the long-lived event
        // frames that follow — those are read inside that task, under no
        // deadline.
        let mut subscription = crate::daemon_bridge::bounded_reply(
            "SubscribeEvents for coordinator readiness",
            self.subscribe_events(),
        )
        .await?;
        let (tx, rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_DEPTH);
        // Issue #1028: the subscription is owned by this task and by nothing
        // else, for the reason [`EVENT_QUEUE_DEPTH`] gives — `next_event` is not
        // cancel-safe, and the readiness wait below is a `timeout`, which drops
        // whatever future it is holding when it expires. Draining here means the
        // only future that timeout can drop is the channel's, and an
        // `mpsc::Receiver::recv` is cancel-safe: a partly-read five-byte frame
        // header stays in this task's `read_frame`, so the watch survives a
        // readiness timeout intact rather than misparsing every frame after it.
        tauri::async_runtime::spawn(async move {
            loop {
                let msg = tokio::select! {
                    // This arm is the one thing in the loop that can drop a
                    // partly-read `next_event`, and it is harmless where the
                    // readiness wait was not: `closed()` resolving means the
                    // receiver is gone, so the subscription is dropped with it
                    // and a desynchronised stream has no reader left to
                    // mislead. The arm also keeps the socket's lifetime
                    // what it was before this split — dropping the watch used to
                    // drop the `EventSubscription` and half-close immediately,
                    // and without this arm the task would sit in `read_frame`
                    // holding the connection open until the daemon next
                    // broadcast something, which on an idle daemon is never.
                    _ = tx.closed() => break,
                    event = subscription.next_event() => match event {
                        Ok(Some(msg)) => msg,
                        // An error and a clean end are both "no more readiness
                        // signals on this stream", and the waiter reports them
                        // the same way — see the `None` arm below. Closing the
                        // channel is how it is told.
                        Ok(None) | Err(_) => break,
                    },
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        });
        Ok(rx)
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
                match watch.recv().await {
                    Some(BroadcastMsg::Event(event))
                        if event.event_type == EventType::SessionStart
                            && event.pane_id.as_deref() == Some(pane_id)
                            && event.agent_id.as_deref() == Some(agent_id) =>
                    {
                        return Ok(Some(event.session_id));
                    }
                    Some(_) => continue,
                    // Issue #1028: the reader task ended the channel — the
                    // daemon ended the stream, or the subscription errored.
                    // Those two used to carry different text and the second is
                    // now folded into the first; the CALLER's behaviour is
                    // unchanged, because `deliver_coordinator_prompt` treats
                    // every `Err` the same way: wait out the rest of the
                    // readiness budget and deliver the seed with no session id.
                    None => return Err("coordinator readiness stream ended".to_string()),
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

/// PRD #1223 audit F4: how long one role's start may take — the whole
/// operation, which for a configured role is `start_prepared_role`'s fresh
/// handshake AND its start reply — before the launch stops waiting for it and
/// treats the role as failed.
///
/// [`crate::daemon_bridge::DECK_REPLY_TIMEOUT`], for that constant's reason: a
/// responsive deck answers a start in milliseconds (the spawn is synchronous,
/// and the prepared-start check runs in the deck's bounded blocking pool), so
/// this is headroom, not a tuned value. Without it a deck that took the
/// connection and never answered left the roles already started running for as
/// long as the peer held the socket, with the rollback that would stop them
/// queued behind the wait.
const WORKFLOW_ROLE_START_TIMEOUT: Duration = crate::daemon_bridge::DECK_REPLY_TIMEOUT;

/// PRD #1223 audit F4: how long ONE rollback stop may take. Each stop is
/// bounded on its own, so a stop the deck never answers costs this and the
/// rollback moves on to the next role instead of waiting behind it.
const WORKFLOW_ROLE_STOP_TIMEOUT: Duration = crate::daemon_bridge::DECK_REPLY_TIMEOUT;

/// A role a launch started (or found started by reconciliation), which a
/// rollback must stop.
#[derive(Debug, Clone)]
struct StartedRole {
    agent_id: String,
    role: String,
}

/// A role a rollback could not confirm is stopped, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnconfirmedStop {
    role: String,
    reason: String,
}

/// What [`rollback_workflow_agents`] did: how many roles it was asked to stop,
/// and EVERY one whose stop it could not confirm.
#[derive(Debug)]
struct RollbackOutcome {
    attempted: usize,
    unconfirmed: Vec<UnconfirmedStop>,
}

impl RollbackOutcome {
    /// The sentence a launch error ends with.
    fn describe(&self) -> String {
        if self.unconfirmed.is_empty() {
            return format!("stopped {} already-started role(s)", self.attempted);
        }
        format!(
            "cleanup could not confirm stop for {} of {} already-started role(s): {}",
            self.unconfirmed.len(),
            self.attempted,
            self.unconfirmed
                .iter()
                .map(|stop| format!("{} ({})", safe_message(&stop.role), stop.reason))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Stop every started role, newest first.
///
/// Each stop is bounded by [`WORKFLOW_ROLE_STOP_TIMEOUT`] on its own (PRD #1223
/// audit F4), and a stop that fails or times out is recorded and the rollback
/// CONTINUES — the roles behind a wedged stop are still stopped, and the
/// outcome names every role whose stop was not confirmed rather than the first.
async fn rollback_workflow_agents<D: WorkflowDaemon + Sync>(
    daemon: &D,
    started: &[StartedRole],
) -> RollbackOutcome {
    let mut unconfirmed = Vec::new();
    for started_role in started.iter().rev() {
        if let Some(stop) = bounded_role_stop(daemon, started_role).await {
            unconfirmed.push(stop);
        }
    }
    RollbackOutcome {
        attempted: started.len(),
        unconfirmed,
    }
}

/// One role's stop under [`WORKFLOW_ROLE_STOP_TIMEOUT`]: `None` when the deck
/// confirmed it, otherwise the role and why it is not confirmed — refused, or
/// not answered within the bound. Shared by the rollback, which stops roles
/// one after another, and by [`stop_roles_concurrently`].
async fn bounded_role_stop<D: WorkflowDaemon + Sync>(
    daemon: &D,
    role: &StartedRole,
) -> Option<UnconfirmedStop> {
    let reason = match tokio::time::timeout(
        WORKFLOW_ROLE_STOP_TIMEOUT,
        daemon.stop_workflow_agent(&role.agent_id),
    )
    .await
    {
        Ok(Ok(())) => return None,
        Ok(Err(error)) => format!("{}: {}", safe_message(&role.agent_id), safe_message(error)),
        Err(_) => format!(
            "{}: the deck did not answer the stop within {}s",
            safe_message(&role.agent_id),
            WORKFLOW_ROLE_STOP_TIMEOUT.as_secs()
        ),
    };
    Some(UnconfirmedStop {
        role: role.role.clone(),
        reason,
    })
}

/// PRD #1223 U4 — stop every one of `roles` at once, each under its own
/// [`WORKFLOW_ROLE_STOP_TIMEOUT`], and return one outcome per role, aligned
/// with `roles`: `None` for a confirmed stop, otherwise why it is not.
///
/// Concurrent, as the TUI's Ctrl+W closes a tab's panes
/// (`close_panes_concurrently`): the stops are independent, so a wedged one
/// costs one bound for the whole close rather than one per role behind it.
async fn stop_roles_concurrently<D: WorkflowDaemon + Sync>(
    daemon: &D,
    roles: &[StartedRole],
) -> Vec<Option<UnconfirmedStop>> {
    join_ordered(
        roles
            .iter()
            .map(|role| bounded_role_stop(daemon, role))
            .collect(),
    )
    .await
}

/// Drive every future in `futures` concurrently on this task and return their
/// outputs in the order given. The crate's one join-all, kept here rather than
/// taking a `futures` dependency for it; each future is polled only until it
/// completes.
async fn join_ordered<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut futures: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_some() {
                continue;
            }
            match future.as_mut().poll(cx) {
                std::task::Poll::Ready(value) => *output = Some(value),
                std::task::Poll::Pending => pending = true,
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs
        .into_iter()
        .map(|output| output.expect("every future completed"))
        .collect()
}

/// One role's start under [`WORKFLOW_ROLE_START_TIMEOUT`], with the elapsed
/// case reported as an ordinary — and INDETERMINATE — start failure, which is
/// what sends it through the caller's reconciliation, so a start that landed
/// although its reply never arrived is found and stopped with the rest.
async fn bounded_role_start<T>(
    start: impl std::future::Future<Output = Result<T, RoleStartFailure>>,
) -> Result<T, RoleStartFailure> {
    match tokio::time::timeout(WORKFLOW_ROLE_START_TIMEOUT, start).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(failure)) => Err(failure),
        Err(_) => Err(RoleStartFailure {
            message: format!(
                "the deck did not answer the start within {}s",
                WORKFLOW_ROLE_START_TIMEOUT.as_secs()
            ),
            indeterminate: true,
        }),
    }
}

/// Why one role's start failed, and whether the deck may still act on it (PRD
/// #1223 audit V5).
///
/// `indeterminate` is what the reconciliation that follows reads: a start whose
/// outcome is unknown may still be spawned by the deck after the reconciliation
/// looked, so one the deck does not list is a start this launch cannot vouch
/// for rather than one that did not happen.
#[derive(Debug)]
struct RoleStartFailure {
    message: String,
    /// `true` when the request may have reached the deck and its outcome is
    /// unknown. `false` only when the outcome is known: the deck ANSWERED with
    /// a refusal, or the client never sent the request.
    indeterminate: bool,
}

impl RoleStartFailure {
    /// How a client error classifies.
    ///
    /// **Definitive** — the deck's own answer, or a request that was never
    /// written:
    ///
    /// * [`ClientError::Server`] is a refusal the deck composed, so it read the
    ///   request and started nothing;
    /// * [`ClientError::SocketMissing`] is decided before a connection exists.
    ///
    /// **Indeterminate** — everything else, because nothing here can tell a
    /// connection that failed BEFORE the request was written from one that
    /// failed after the deck had read it:
    ///
    /// * [`ClientError::Io`] covers both, and the second is exactly the lost
    ///   reply this classification exists for;
    /// * [`ClientError::Malformed`] is the one that carries the lost reply in
    ///   practice — a deck that reads the request and then drops the connection
    ///   ends the client's read at `daemon closed connection before sending
    ///   RESP`, which is this variant and not `Io`. It also covers a reply this
    ///   client could not decode, which says nothing about what the deck did.
    ///
    /// Erring toward indeterminate costs a cleanup warning the user may not
    /// have needed; erring the other way loses a role that is running.
    fn from_client(error: ClientError) -> Self {
        let indeterminate = !matches!(
            error,
            ClientError::Server(_) | ClientError::SocketMissing(_)
        );
        Self {
            message: safe_message(error.to_string()),
            indeterminate,
        }
    }
}

/// Why [`launch_configured_orchestration`] or [`launch_workflow`] failed: the
/// whole sentence, and — as data, not prose (PRD #1223 audits F6 and V2) —
/// every role it could not confirm is stopped, which
/// [`crate::dto::DesktopActionError::launch`] carries to the webview.
#[derive(Debug)]
struct LaunchFailure {
    message: String,
    unconfirmed_stops: Vec<String>,
}

impl LaunchFailure {
    /// A failure after `rollback`, plus the role a reconciliation could not
    /// vouch for, if any.
    fn after_rollback(
        message: String,
        rollback: &RollbackOutcome,
        uncertain: Option<&UnconfirmedStop>,
    ) -> Self {
        Self {
            message,
            unconfirmed_stops: rollback
                .unconfirmed
                .iter()
                .chain(uncertain)
                .map(|stop| stop.role.clone())
                .collect(),
        }
    }
}

impl From<String> for LaunchFailure {
    /// A failure before anything started, so there is nothing to confirm.
    fn from(message: String) -> Self {
        Self {
            message,
            unconfirmed_stops: Vec::new(),
        }
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
) -> Result<WorkflowLaunchResult, LaunchFailure> {
    validate_desktop_coordinator(roles)?;
    let created_at = daemon.now();
    // Subscribe before the first spawn so a fast Claude SessionStart cannot be
    // lost between process creation and readiness observation.
    let mut readiness = daemon.begin_coordinator_readiness().await?;
    let mut started = Vec::with_capacity(roles.len());
    let mut start_target = None;

    for (role_index, role) in roles.iter().enumerate() {
        // PRD #819 audit follow-up: each role's spawn opens its OWN short-lived
        // connection, one that exchanges no `Hello` and re-checks no protocol
        // version. That used to be a fail-open — the token rode on the stable
        // `start-agent` op as an additive key, so an older daemon substituted
        // since the preparation accepted the request, ignored the key and
        // started the role unenforced. The preceding round of this fix verified
        // the peer on a separate connection first, which narrowed the window to
        // the gap between two `connect()` calls without shutting it.
        //
        // It is shut here instead, and one hop lower: a token routes this spawn
        // onto `start-prepared-agent`, a verb such a daemon does not have, so it
        // fails the frame decode and answers `ok: false` on the spawn's own
        // connection. The refusal below is that answer, and it rolls back what
        // has already started. A token-less start is byte-for-byte unchanged and
        // spends no extra round trip on a preparation it does not have.
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
        // PRD #1223 audit F4: bounded, as the New agent flow's configured
        // starts are — the rollback below is shared, and a wedged start would
        // otherwise hold every role already started running behind it.
        match bounded_role_start(daemon.start_workflow_agent(options, prep_token)).await {
            Ok(agent_id) => {
                if role.start {
                    start_target = Some((pane_id, agent_id.clone()));
                }
                started.push(StartedRole {
                    agent_id,
                    role: role.role.clone(),
                });
            }
            Err(failure) => {
                // StartAgent can spawn/register successfully and then lose its
                // response. Reconcile the already-known pane + orchestration
                // identity before rollback so that just-spawned role is not
                // leaked merely because its id never reached this client.
                let uncertain = reconcile_failed_start(
                    daemon,
                    &mut started,
                    &pane_id,
                    orchestration_id,
                    &role.role,
                    failure.indeterminate,
                )
                .await;
                let reconciliation_note = uncertain
                    .as_ref()
                    .map(|stop| format!("; cleanup uncertainty: {}", stop.reason))
                    .unwrap_or_default();
                let rollback = rollback_workflow_agents(daemon, &started).await;
                // PRD #1223 audit V2: the roles it could not confirm travel as
                // data, as the New agent launch's do, so the Runs screen puts
                // the cleanup warning ahead of any refusal code this sentence
                // also carries — a `stale-preparation:` refusal after a role
                // started is exactly such a composite.
                return Err(LaunchFailure::after_rollback(
                    format!(
                        "failed to start workflow role {}: {}; {}{reconciliation_note}",
                        safe_message(&role.role),
                        safe_message(failure.message),
                        rollback.describe(),
                    ),
                    &rollback,
                    uncertain.as_ref(),
                ));
            }
        }
    }

    let Some((start_pane_id, start_agent_id)) = start_target else {
        let rollback = rollback_workflow_agents(daemon, &started).await;
        return Err(LaunchFailure::after_rollback(
            format!(
                "validated workflow did not start a coordinator; {}",
                rollback.describe()
            ),
            &rollback,
            None,
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
        let rollback = rollback_workflow_agents(daemon, &started).await;
        return Err(LaunchFailure::after_rollback(
            format!(
                "workflow coordinator context delivery failed: {}; {}",
                safe_message(error),
                rollback.describe()
            ),
            &rollback,
            None,
        ));
    }

    Ok(WorkflowLaunchResult {
        start_agent_id,
        agent_ids: started.into_iter().map(|role| role.agent_id).collect(),
    })
}

/// After a role's start failed: look the role up by the pane and orchestration
/// identity the start carried, so a spawn that landed although its reply was
/// lost — or never arrived, because the start timed out — is added to
/// `started` and stopped by the rollback like the others.
///
/// `Some` is a role this launch cannot vouch for: the lookup itself failed, or
/// — after an INDETERMINATE failure (PRD #1223 audit V5), one whose request may
/// have reached the deck — it found nothing although the deck may still spawn
/// the role once whatever held its reply clears. Either way the rollback will
/// not stop it, so the caller reports it as cleanup it could not confirm.
async fn reconcile_failed_start<D: WorkflowDaemon + Sync>(
    daemon: &D,
    started: &mut Vec<StartedRole>,
    pane_id: &str,
    orchestration_id: &str,
    role: &str,
    indeterminate: bool,
) -> Option<UnconfirmedStop> {
    match daemon
        .reconcile_workflow_agent(pane_id, orchestration_id, COORDINATOR_DELIVERY_RPC_TIMEOUT)
        .await
    {
        Ok(Some(agent_id)) => {
            if !started.iter().any(|known| known.agent_id == agent_id) {
                started.push(StartedRole {
                    agent_id,
                    role: role.to_string(),
                });
            }
            None
        }
        Ok(None) if !indeterminate => None,
        Ok(None) => Some(UnconfirmedStop {
            role: role.to_string(),
            reason: "the role's start was not answered, so the deck may have received it, and \
                     the deck did not list the role afterwards; if it starts late it will not \
                     be stopped"
                .to_string(),
        }),
        Err(reconciliation_error) => Some(UnconfirmedStop {
            role: role.to_string(),
            reason: format!(
                "could not reconcile the failed role by pane and orchestration identity: {}",
                safe_message(reconciliation_error)
            ),
        }),
    }
}

/// What an orchestration launch aimed at a deck that cannot start a role with
/// its configured command answers (PRD #1223 M6), and the reason the New agent
/// form shows in place of the orchestration chips on such a deck.
const CONFIGURED_ROLE_COMMAND_UNSUPPORTED: &str = "This deck cannot start orchestration roles with their configured commands, so its orchestrations are not offered here. Nothing was started. Launch them from the TUI on that deck's host, or upgrade the deck.";

/// One role of a configured orchestration launch (PRD #1223 M6), as
/// `StartPreparedAgent` with `use_configured_command` receives it: no
/// `command`, no `agent_type` and no `seed`, because the deck takes all three
/// from the role's config — sending any of them is refused.
///
/// What the request does carry is the TUI's role-spawn identity
/// (`src/tab.rs`): the role name as the pane name and membership role, its start
/// marker, its index, the orchestration and its directory, ONE orchestration id
/// shared by every role, and the run's title — which, like the TUI's, is absent
/// when the Name is empty so the tab falls back to the orchestration's name.
#[allow(clippy::too_many_arguments)]
fn configured_role_start_options(
    orchestration: &str,
    cwd: &str,
    role: &ProjectRole,
    role_index: usize,
    orchestration_id: &str,
    display_title: Option<&str>,
    pane_id: String,
    rows: u16,
    cols: u16,
) -> StartAgentOptions {
    StartAgentOptions {
        command: None,
        cwd: Some(cwd.to_string()),
        display_name: Some(role.name.clone()),
        rows,
        cols,
        env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id)],
        tab_membership: Some(TabMembership::Orchestration {
            name: orchestration.to_string(),
            role_index,
            role_name: role.name.clone(),
            is_start_role: role.start,
            orchestration_cwd: Some(cwd.to_string()),
            display_title: display_title.map(str::to_string),
            orchestration_id: Some(orchestration_id.to_string()),
        }),
        agent_type: None,
        seed: None,
    }
}

/// PRD #1223 M6: launch a prepared orchestration the way the TUI's `Ctrl+n`
/// does — every role with the command its project config gives it, started on
/// the deck in the order the preparation listed them.
///
/// It is [`launch_workflow`]'s sibling rather than a mode of it, because the
/// Runs launch's form rules are exactly what this flow must not inherit: that
/// one builds each role's command from desktop agent profiles and refuses a Pi
/// coordinator. What they share is the machinery around the spawns — the
/// readiness subscription taken before the first spawn, the reconcile of a
/// spawn whose reply was lost, the reverse-order rollback, and the
/// acknowledged coordinator delivery.
///
/// # How the start role gets its coordinator prompt
///
/// The same two ways the TUI's does. A **Pi** start role was seeded by the deck
/// at spawn — PRD #201's native delivery, with the deck's own PTY safety net —
/// because the deck, not this client, knows the role is Pi; this reads the type
/// the deck recorded and delivers nothing itself, which would be a second copy.
/// Every **other** start role gets [`deliver_coordinator_prompt`], the Runs
/// launch's readiness-gated, identity-bound submission, and a delivery that
/// fails rolls the launch back as the Runs launch does.
///
/// # A role that cannot be started
///
/// The roles already started are stopped again, in reverse order, and the
/// error names them — the TUI closes the panes it already created on the same
/// failure. A deck that does not advertise `prepared-role-command` is answered
/// by the client library without sending anything, and is reported as that.
#[allow(clippy::too_many_arguments)]
async fn launch_configured_orchestration<D: WorkflowDaemon + Sync>(
    daemon: &D,
    orchestration: &str,
    display_title: Option<&str>,
    prepared: &PreparedWorkflow,
    rows: u16,
    cols: u16,
    orchestration_id: &str,
) -> Result<WorkflowLaunchResult, LaunchFailure> {
    if prepared.roles.iter().filter(|role| role.start).count() != 1 {
        return Err(
            "the deck prepared an orchestration without exactly one start role; nothing was started"
                .to_string()
                .into(),
        );
    }
    let created_at = daemon.now();
    // Subscribed before the first spawn, for `launch_workflow`'s reason: a fast
    // SessionStart must not be lost between the spawn and the wait.
    let mut readiness = daemon.begin_coordinator_readiness().await?;
    let mut started: Vec<StartedRole> = Vec::with_capacity(prepared.roles.len());
    let mut start_target = None;

    for (role_index, role) in prepared.roles.iter().enumerate() {
        let pane_id = mint_desktop_pane_id();
        let options = configured_role_start_options(
            orchestration,
            &prepared.path,
            role,
            role_index,
            orchestration_id,
            display_title,
            pane_id.clone(),
            rows,
            cols,
        );
        // Every role that had started before this one — named in the error
        // whatever the rollback then manages, because it is what the user has
        // to know was touched. Taken before any reconciliation adds the failed
        // role itself.
        let already = if started.is_empty() {
            "no role had started".to_string()
        } else {
            format!(
                "roles already started: {}",
                started
                    .iter()
                    .map(|known| safe_message(&known.role))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        // PRD #1223 audit F4: the whole configured start — the client
        // library's fresh handshake and the start reply — is one bounded
        // operation, so a deck that never answers role N cannot hold roles
        // 1..N-1 running behind it.
        let (error, uncertain) = match bounded_role_start(
            daemon.start_configured_role(options, &prepared.token),
        )
        .await
        {
            Ok(GatedQuery::Answered(agent_id)) => {
                if role.start {
                    start_target = Some((pane_id, agent_id.clone()));
                }
                started.push(StartedRole {
                    agent_id,
                    role: role.name.clone(),
                });
                continue;
            }
            // Withheld by the client library: nothing reached the deck for this
            // role, so there is nothing to reconcile.
            Ok(GatedQuery::Unsupported) => (CONFIGURED_ROLE_COMMAND_UNSUPPORTED.to_string(), None),
            // A spawn can succeed and lose its reply — or, when the start timed
            // out, still be pending on the deck; reconcile by pane and
            // orchestration identity so a role that landed is stopped too.
            Err(failure) => {
                let uncertain = reconcile_failed_start(
                    daemon,
                    &mut started,
                    &pane_id,
                    orchestration_id,
                    &role.name,
                    failure.indeterminate,
                )
                .await;
                (safe_message(failure.message), uncertain)
            }
        };
        let rollback = rollback_workflow_agents(daemon, &started).await;
        let reconciliation_note = uncertain
            .as_ref()
            .map(|stop| format!("; cleanup uncertainty: {}", stop.reason))
            .unwrap_or_default();
        return Err(LaunchFailure::after_rollback(
            format!(
                "failed to start orchestration role {}: {error}; {already}; {}{reconciliation_note}",
                safe_message(&role.name),
                rollback.describe(),
            ),
            &rollback,
            uncertain.as_ref(),
        ));
    }

    let Some((start_pane_id, start_agent_id)) = start_target else {
        let rollback = rollback_workflow_agents(daemon, &started).await;
        return Err(LaunchFailure::after_rollback(
            format!(
                "the orchestration started no coordinator; {}",
                rollback.describe()
            ),
            &rollback,
            None,
        ));
    };
    let delivered_by_deck = match daemon.launched_agent_type(&start_agent_id).await {
        Ok(agent_type) => agent_type == Some(AgentType::Pi),
        Err(error) => {
            let rollback = rollback_workflow_agents(daemon, &started).await;
            return Err(LaunchFailure::after_rollback(
                format!(
                    "could not tell how the coordinator receives its context: {}; {}",
                    safe_message(error),
                    rollback.describe()
                ),
                &rollback,
                None,
            ));
        }
    };
    if !delivered_by_deck
        && let Err(error) = deliver_coordinator_prompt(
            daemon,
            &mut readiness,
            &start_pane_id,
            &start_agent_id,
            &prepared.prompt,
            created_at,
        )
        .await
    {
        let rollback = rollback_workflow_agents(daemon, &started).await;
        return Err(LaunchFailure::after_rollback(
            format!(
                "orchestration coordinator context delivery failed: {}; {}",
                safe_message(error),
                rollback.describe()
            ),
            &rollback,
            None,
        ));
    }

    Ok(WorkflowLaunchResult {
        start_agent_id,
        agent_ids: started.into_iter().map(|role| role.agent_id).collect(),
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
        .unwrap_or_else(|| "the local deck did not become connected".into()))
}

async fn refresh_and_emit(app: &AppHandle, links: &DaemonLinks) -> DesktopSnapshot {
    let snapshot = get_snapshot(links).await;
    emit_snapshot(app, &snapshot);
    snapshot
}

/// Queue depth between the subscription reader and the refresh loop.
///
/// PRD #741 M4(b) split the two because [`EventSubscription::next_event`] is
/// **not cancel-safe** — `read_frame` accumulates a five-byte header across
/// awaits into a local buffer, so a `select!` arm that drops the future mid-read
/// loses those bytes and desynchronises the stream. An `mpsc::Receiver::recv` is
/// cancel-safe and an `Interval::tick` is, so the reader owns the subscription
/// exclusively and the loop selects on the channel instead.
///
/// Bounded rather than unbounded: a refresh loop that stalls must apply
/// backpressure to the socket rather than grow without limit. 512 is generous
/// against what it replaces — before this split the loop read at most one event
/// per full `get_snapshot()`, i.e. ~6.667/s, so any depth at all is an
/// improvement in how fast the desktop drains the daemon's broadcast.
const EVENT_QUEUE_DEPTH: usize = 512;

/// Start a watcher for every observed deck that has none (PRD #742 M3).
///
/// # One watcher per deck, and what that makes structural
///
/// #741 hit a **stale fold**: one watcher followed the selection, its event
/// subscription was a connection to exactly one daemon, and nothing about a
/// selection change ended it — so the previous deck's broadcasts went on being
/// folded into the view answering the new deck's snapshots. That fix was a
/// signal the watcher had to be told about. With N decks the hazard generalises
/// and gets worse, because the subscriptions are now **concurrent** rather than
/// sequential and `BroadcastMsg` carries no deck id at all.
///
/// So the isolation is built rather than remembered: each deck's watcher owns
/// its own [`AgentView`], so a fold cannot reach another deck's view because it
/// is in another task's object; and each stamps its own endpoint on what it
/// emits, so a record cannot be labelled with another deck's name because the
/// name comes from the task's own endpoint rather than from `selected_endpoint()`.
///
/// Per-deck watchers also settle the two things the PRD asks for by
/// construction: **retry and backoff are per deck**, so one unreachable deck
/// cannot starve the others, and **coalescing stays per watcher** — the
/// alternative, one coalescer over N streams, shares its 150 ms window across
/// decks and reintroduces exactly the cross-deck stall this milestone exists to
/// remove.
///
/// # Idempotent, and called from every path that can change the set
///
/// The claim is [`DesktopState::start_watcher_once_for`], so calling this again
/// for a deck that already has a watcher does nothing. The bootstrap paths call
/// it because they are where the app first has a deck to watch; `apply_selection`
/// calls it because a settings save can add a deck to the fleet **without**
/// moving the resolved selection — which is the "the set gained a member" signal
/// M2 derived and deliberately left without a consumer.
fn ensure_snapshot_watchers(app: &AppHandle, state: &DesktopState) {
    for endpoint in crate::dto::observed_decks() {
        spawn_deck_watcher(app, state, endpoint);
    }
}

/// The watcher loop for **one** deck.
fn spawn_deck_watcher(app: &AppHandle, state: &DesktopState, endpoint: Endpoint) {
    let key = endpoint.identity();
    // PRD #742 M8: the claim's TOKEN, carried to `register_watcher` below so it
    // can tell this claim from one a later `start_watcher_once_for` made for the
    // same deck. `None` means somebody else already holds the claim.
    let Some(claim) = state.start_watcher_once_for(&key) else {
        return;
    };
    let app = app.clone();
    // PRD #741 M4(a): the watcher holds its own handle on the link store. It is
    // the loop this milestone exists for — it is the thing that was paying two
    // connections per refresh at up to 6.667 refreshes a second — and it is
    // also the only place that can observe a daemon being replaced, because its
    // event subscription is the one connection the desktop holds open across
    // refreshes. Hence the `invalidate` below.
    let links = Arc::clone(&state.daemon);
    // PRD #741 M9: the selection signal. Subscribed here rather than inside the
    // task so the first observed generation is the one in force when the
    // watcher started, not whatever it happens to be when the task is polled.
    let mut selection = state.selection.subscribe();
    // PRD #1223 M3: this deck's refetch nudge, subscribed here for the same
    // reason the selection is — a start that lands before the task is first
    // polled must still be an edge the loop observes.
    let mut refetch = state.refetch_signal(&key);
    let handle = tauri::async_runtime::spawn(async move {
        // PRD #741 M4(b): the incremental agent list. It belongs to this task
        // and to nothing else — it is only ever correct while this task's
        // subscription is the one feeding it, so a second holder could not be
        // told whether its contents were live. PRD #742 M3: that is now also
        // what keeps one deck's fold out of another's, since there is one of
        // these per deck and a fold cannot reach across two objects.
        let mut view = AgentView::default();
        loop {
            let daemon = match links.trusted(&endpoint).await {
                Ok(daemon) if daemon.require_compatible().is_ok() => daemon,
                _ => {
                    view.resubscribed();
                    let snapshot = snapshot_with(&endpoint, &links, None).await;
                    emit_snapshot(&app, &snapshot);
                    tokio::time::sleep(WATCH_RETRY_DELAY).await;
                    continue;
                }
            };
            // PRD #742 M14: bounded, like the handshake before it and the
            // `ListAgents` after it. Only the RESP that CONFIRMS the
            // subscription is bounded — the event frames that follow are
            // long-lived by design and are read by `spawn_event_reader`.
            let subscription = match crate::daemon_bridge::bounded_reply(
                "SubscribeEvents",
                daemon.client.subscribe_events(),
            )
            .await
            {
                Ok(subscription) => subscription,
                Err(_) => {
                    // Could not even subscribe against a link that just said it
                    // was compatible: drop it rather than retry through it.
                    view.resubscribed();
                    // PRD #742 M3: THIS deck's link, not every deck's. A
                    // subscription that failed says nothing about the other
                    // machines in the fleet, and dropping their links would make
                    // one deck's bad moment cost N handshakes.
                    links.invalidate(&endpoint).await;
                    let snapshot = snapshot_with(&endpoint, &links, None).await;
                    emit_snapshot(&app, &snapshot);
                    tokio::time::sleep(WATCH_RETRY_DELAY).await;
                    continue;
                }
            };
            // A NEW subscription means the fold has a hole in it of unknown
            // size, so everything held is discarded and the first refresh under
            // this stream re-fetches. Called before the first event can arrive.
            view.resubscribed();
            // PRD #741 M9: anything announced before this subscription existed
            // is already accounted for by the establishment above, so the arm
            // starts from here rather than firing once on a stale edge.
            selection.mark_unchanged();
            let reader = spawn_event_reader(subscription);
            let ended = watch_one_subscription(
                &app,
                &endpoint,
                &links,
                &mut view,
                reader,
                &mut selection,
                &mut refetch,
            )
            .await;
            // PRD #741 M4(a): the event stream ended. That is this watcher's
            // long-lived connection to its daemon going away, and a daemon
            // cannot be replaced without the old process dying and taking this
            // socket with it — so this is the signal that the held handshake may
            // now describe a process that no longer exists. Drop the link before
            // reconnecting; the loop's next `trusted` handshakes against
            // whatever is actually there now.
            //
            // A selection change reaches the same place for a different reason:
            // the link is not stale, it simply describes a deck whose applied
            // selection state has moved. `apply_selection` has already
            // invalidated it, and the call below is a no-op in that case rather
            // than a second mechanism.
            links.invalidate(&endpoint).await;
            // PRD #741 M9: the retry delay is a backoff for a deck that is not
            // answering. A selection change is a user's click, and there is a
            // healthy deck waiting at the other end of it, so it re-subscribes
            // straight away — a second of dead air after choosing a deck is the
            // whole of what the user would see.
            if ended == SubscriptionEnd::SelectionChanged {
                continue;
            }
            tokio::time::sleep(WATCH_RETRY_DELAY).await;
        }
    });
    state.register_watcher(&key, claim, handle);
}

/// Drain one subscription into a channel until it ends.
///
/// A task of its own so nothing can cancel a partially-read frame — see
/// [`EVENT_QUEUE_DEPTH`].
fn spawn_event_reader(
    mut subscription: EventSubscription,
) -> tokio::sync::mpsc::Receiver<BroadcastMsg> {
    let (tx, rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_DEPTH);
    tauri::async_runtime::spawn(async move {
        while let Ok(Some(msg)) = subscription.next_event().await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
        // Dropping `tx` here is what tells the refresh loop the stream ended,
        // and it happens after every already-read event has been delivered.
    });
    rx
}

/// Why [`watch_one_subscription`] returned (PRD #741 M9).
///
/// The two are not the same event and must not share a retry policy: a stream
/// that ended is a deck that may be gone, and backing off is right; a selection
/// change is a click, and backing off is a second of dead air the user reads as
/// the app ignoring them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionEnd {
    /// The daemon's event stream ended — EOF, an error, or a replaced daemon.
    Ended,
    /// The user chose a different deck, so this subscription is against the
    /// wrong one.
    SelectionChanged,
}

/// The refresh loop for one subscription: fold what arrives, re-emit at the
/// coalesce floor, and wake on the reconciliation timer even when nothing
/// arrives at all.
///
/// Returns when the subscription ends, or when the selected deck changes.
///
/// PRD #742 M3: `endpoint` is the deck this subscription is against, and it is
/// the only deck this call ever names — the fold it fills, the events it stamps
/// and the snapshot it emits are all that deck's.
async fn watch_one_subscription(
    app: &AppHandle,
    endpoint: &Endpoint,
    links: &DaemonLinks,
    view: &mut AgentView,
    mut events: tokio::sync::mpsc::Receiver<BroadcastMsg>,
    selection: &mut tokio::sync::watch::Receiver<u64>,
    refetch: &mut tokio::sync::watch::Receiver<u64>,
) -> SubscriptionEnd {
    let mut reconcile = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately on its first tick; the refresh below already
    // fetches because a fresh view demands it, so consume that one here rather
    // than paying for it twice.
    reconcile.tick().await;
    // Deliberately NOT `mark_unchanged()`, unlike the selection: a nudge that
    // landed between subscriptions should wake this loop into its first
    // refresh now rather than leave the deck unlisted until the next event or
    // reconcile tick. The fresh view makes that refresh a fetch either way.
    //
    // Cleared when the sender is gone — `retain_watchers` dropped this deck and
    // is aborting this task — so a closed channel cannot complete the arm on
    // every pass and spin the loop.
    let mut refetch_open = true;

    let mut last_refresh: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            msg = events.recv() => match msg {
                Some(msg) => {
                    emit_daemon_event(app, endpoint, &msg);
                    view.apply(&msg);
                }
                // The reader task is gone, so the subscription ended.
                None => return SubscriptionEnd::Ended,
            },
            _ = reconcile.tick() => view.mark_reconcile_due(),
            // PRD #1223 M3: a deck-targeted start just spawned an agent here,
            // and the daemon's `StartAgent` handler broadcasts nothing — so the
            // fold cannot know about it and the next emit has to be a fresh
            // listing. See
            // `DesktopState::refetch`.
            changed = refetch.changed(), if refetch_open => match changed {
                Ok(()) => view.mark_reconcile_due(),
                Err(_) => refetch_open = false,
            },
            // PRD #741 M9: returns BEFORE the refresh below, deliberately, and
            // the shape is kept — but PRD #742 M3 changed what it buys, so the
            // reason is restated rather than inherited.
            //
            // It used to be the thing that stopped a MISLABEL: one watcher
            // followed the selection, so the fold under this arm was built from
            // the deck the user had just left and `snapshot_with(&selected_endpoint(), …)`
            // would have answered the NEW deck's snapshot out of it — one
            // machine's agents under another machine's name. That is no longer
            // what prevents it. This task's `endpoint` is fixed for its whole
            // life, so the fold and the label come from the same deck by
            // construction and could not disagree however late this arm fired.
            //
            // What returning still buys is the re-establishment, and it is a
            // real one: `retarget_selection` has just called
            // `DaemonLinks::invalidate_all`, so the link behind this
            // subscription is one the app has deliberately forgotten. Returning
            // discards the fold with the subscription that filled it, and
            // `view.resubscribed()` runs before the next stream can deliver
            // anything. The distinct return type is what keeps this off
            // `WATCH_RETRY_DELAY`: a selection change is a click with a healthy
            // deck behind it, and a second of dead air is the whole of what the
            // user would see.
            //
            // `changed()` errors only when every sender is gone, which cannot
            // happen while `DesktopState` is alive; treated as "no more
            // selection changes" rather than as a reason to end the watch.
            changed = selection.changed() => {
                if changed.is_ok() {
                    return SubscriptionEnd::SelectionChanged;
                }
            }
        }
        // Everything already queued is applied to THIS refresh rather than
        // costing one of its own. Before M4(b) each event cost a full
        // `ListAgents` and a 150 ms wait, so a burst of N drained at 6.667/s;
        // now a burst of N is N folds and one emit.
        drain_pending(app, endpoint, view, &mut events);
        if let Some(previous) = last_refresh {
            let elapsed = previous.elapsed();
            if elapsed < SNAPSHOT_COALESCE_INTERVAL {
                tokio::time::sleep(SNAPSHOT_COALESCE_INTERVAL - elapsed).await;
                // The sleep is the coalescing window: whatever landed during it
                // belongs to the snapshot about to be emitted.
                drain_pending(app, endpoint, view, &mut events);
            }
        }
        // PRD #741, Greptile P1 on #1035: the `select!` arm above observes a
        // selection change only while this loop is PARKED in it. Everything
        // from there to the emit runs outside it — `drain_pending`, and above
        // all the coalescing sleep, which is up to `SNAPSHOT_COALESCE_INTERVAL`
        // of wall clock — so a change landing in that stretch goes unseen until
        // the next iteration, by which time the emit has already happened.
        //
        // PRD #742 M3 narrowed what that costs, and the old note overstated it
        // from here on. It said the late emit "would have paired the NEW deck's
        // `selected_endpoint()` with a `view` folded from the OLD one". That was
        // true of a watcher that followed the selection; this one does not, so
        // the emit below pairs THIS deck's endpoint with THIS deck's fold
        // whenever it happens to run. What the poll still buys is promptness —
        // the re-establishment the arm above exists for, taken at the end of
        // this pass rather than one coalescing window later.
        //
        // Kept rather than removed because it is also the stronger of the two
        // observations: an arm only covers the window it is racing, while one
        // poll immediately before the emit covers everything since the last
        // observation — the drain, the sleep, and the snapshot decision itself.
        if selection_moved_since_last_seen(selection) {
            return SubscriptionEnd::SelectionChanged;
        }
        let snapshot = snapshot_with(endpoint, links, Some(view)).await;
        emit_snapshot(app, &snapshot);
        last_refresh = Some(tokio::time::Instant::now());
    }
}

/// Has the selection moved since this receiver last observed it?
///
/// A named function rather than the one call inlined, because the two things
/// worth pinning about it are both decisions and neither is visible at the call
/// site (PRD #741, Greptile P1 on #1035).
///
/// **Polled rather than a second `select!` arm on the sleep**, and that is the
/// stronger of the two: an arm would only cover the window it is racing, while
/// one poll immediately before the emit covers *everything* since the last
/// observation — the drain, the sleep, and the snapshot decision itself.
///
/// **An error reads as `false`, not as a reason to end the watch.**
/// [`tokio::sync::watch::Receiver::has_changed`] errors only when every sender
/// is gone, which cannot happen while `DesktopState` is alive; and an arm that
/// took the error as "changed" would complete immediately and forever, spinning
/// the loop instead of coalescing it.
///
/// Returning without marking the value seen is correct: the caller runs
/// `selection.mark_unchanged()` before the next subscription, so the next
/// `watch_one_subscription` starts from the generation actually in force.
fn selection_moved_since_last_seen(selection: &tokio::sync::watch::Receiver<u64>) -> bool {
    selection.has_changed().unwrap_or(false)
}

/// Apply every event already queued, without waiting for another.
fn drain_pending(
    app: &AppHandle,
    endpoint: &Endpoint,
    view: &mut AgentView,
    events: &mut tokio::sync::mpsc::Receiver<BroadcastMsg>,
) {
    while let Ok(msg) = events.try_recv() {
        emit_daemon_event(app, endpoint, &msg);
        view.apply(&msg);
    }
}

/// One daemon broadcast, forwarded to the webview **stamped with the deck it
/// came from** (PRD #742 M3).
///
/// # The stamp
///
/// `BroadcastMsg` carries no deck id and this event was emitted unwrapped, so
/// "which deck is this event from" was answerable only by "there is exactly
/// one". With a watcher per observed deck that stops being true, and the raw
/// event stream would have stayed single-deck under a multi-deck view — the
/// drawer's evidence list and its handoff edges are both built from this stream.
///
/// # Additive on the wire, deliberately
///
/// `deck` is **flattened beside** the message rather than wrapping it, so the
/// payload keeps `kind` and every snake_case `AgentEvent` field exactly where
/// `desktop/src/lib/daemonEvents.ts` reads them today. A wrapper would have been
/// a frontend change, and the frontend is M4's.
///
/// The value is [`crate::dto::deck_wire_id`]'s — the same token
/// `connection.deckId` carries, which is what `bridge.ts` keys `daemonId` on —
/// so a consumer can key an event to the group that a snapshot put on screen
/// without a second naming scheme to keep in step.
///
/// **PRD #742 M5 moved it off `deck_path_text`, and the move is the point.**
/// That was `Endpoint::describe()`, which renders neither the remote socket
/// path, the identity file nor the jump host — so two decks differing only in
/// one of those stamped their events identically, and the webview's
/// "is this event from the deck the screen is on" filter answered yes for the
/// wrong machine's events. The stamp has to track whatever the snapshot's key
/// is, or the filter compares two different naming schemes.
///
/// # Unguarded, and stated rather than glossed
///
/// **No test asserts this**, and none can without a production refactor this
/// milestone declined. Reaching an emit needs an `AppHandle`, which needs a
/// running Tauri app; `tauri::test::MockRuntime` exists as a dev-dependency, but
/// this chain names the concrete `AppHandle<Wry>` throughout, so driving it from
/// a mock app means making `ensure_snapshot_watchers`, `emit_snapshot`,
/// [`watch_one_subscription`], `refresh_and_emit` and [`drain_pending`] generic
/// over `R: Runtime` — a real refactor with headless risk, and not M3's. What a
/// reader should check by hand is one line: with two decks observed, every
/// `desktop://daemon-event` payload in the webview console carries a `deck`
/// equal to the `connection.deckId` of the deck that emitted it.
fn emit_daemon_event(app: &AppHandle, endpoint: &Endpoint, msg: &BroadcastMsg) {
    let _ = app.emit("desktop://daemon-event", DeckStamped::new(endpoint, msg));
}

/// The `desktop://daemon-event` payload: one [`BroadcastMsg`], flattened, with
/// the deck it came from beside it. See [`emit_daemon_event`].
///
/// A named type at module scope rather than a local inside the emit, so that the
/// one half of this change a test **can** reach is reachable: the emit needs a
/// running Tauri app, but the wire shape does not, and the wire shape is what
/// would break `desktop/src/lib/daemonEvents.ts` if `flatten` did not do what
/// this is relying on it to do. Pinned by
/// [`tests::a_stamped_daemon_event_adds_the_deck_and_moves_nothing_else`].
#[derive(Clone, serde::Serialize)]
struct DeckStamped<'a> {
    deck: String,
    #[serde(flatten)]
    event: &'a BroadcastMsg,
}

impl<'a> DeckStamped<'a> {
    fn new(endpoint: &Endpoint, event: &'a BroadcastMsg) -> Self {
        Self {
            deck: crate::dto::deck_wire_id(endpoint),
            event,
        }
    }
}

#[tauri::command]
async fn desktop_get_snapshot(
    app: AppHandle,
    webview: Webview,
    state: State<'_, DesktopState>,
) -> Result<DesktopSnapshot, String> {
    ensure_main_webview(&webview)?;
    Ok(refresh_and_emit(&app, &state.daemon).await)
}

/// PRD #819 M6: the projects THIS DAEMON knows about.
///
/// Every path the desktop offers comes from here or from the user; none is
/// derived from the desktop's own environment, which is the whole invariant.
/// An empty listing is a successful answer — "this daemon has nothing live and
/// its startup cwd is not a project" — and the webview renders its
/// paste-a-path surface for it rather than an error.
#[tauri::command]
async fn desktop_list_projects(
    webview: Webview,
    state: State<'_, DesktopState>,
) -> Result<DesktopProjectListing, String> {
    ensure_main_webview(&webview)?;
    let daemon = trusted_daemon(&state.daemon).await?;
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
    state: State<'_, DesktopState>,
    path: String,
) -> Result<DesktopResolvedProject, String> {
    ensure_main_webview(&webview)?;
    validate_pasted_project_path(&path)?;
    let daemon = trusted_daemon(&state.daemon).await?;
    daemon.require_compatible()?;
    let project = daemon
        .client
        .resolve_project(&path)
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    Ok(map_resolved_project(project))
}

/// PRD #1223 M4: one directory of the deck `deck_id` names, for the New agent
/// dialog's directory step.
///
/// `path` is one the daemon listed (a listing's `path`, `parent` or an entry's
/// `path`), and it goes to the daemon **verbatim**; the
/// reply's canonical spelling is what the dialog carries from then on. `None`
/// asks for the daemon user's home directory. Nothing here derives a path —
/// not a parent by trimming, not a child by joining.
///
/// A deck that predates the verb answers [`DesktopDirectoryListing::Unsupported`]
/// rather than an error. The dialog does not ask one: the connection's
/// `new_agent_reason` disables it at the deck step (PRD #1223 U1).
#[tauri::command]
async fn desktop_list_directories(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: String,
    path: Option<String>,
) -> Result<DesktopDirectoryListing, String> {
    ensure_main_webview(&webview)?;
    list_directories_on(&state, &deck_id, path).await
}

/// PRD #1223 M4: what the New agent form needs to know about the deck
/// `deck_id` names — its default command, its agent registry, its experimental
/// flag, the authoring kinds it can compose — plus the command this app last
/// started a plain agent with there.
///
/// A deck that predates the query answers
/// [`DesktopNewAgentOptions::Unsupported`], carrying this app's own compiled
/// registry for the form to offer instead, labelled as such.
#[tauri::command]
async fn desktop_new_agent_options(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: String,
) -> Result<DesktopNewAgentOptions, String> {
    ensure_main_webview(&webview)?;
    new_agent_options_on(&state, &deck_id).await
}

/// [`desktop_list_directories`] minus the webview, so a test can drive it
/// against real daemons.
///
/// # The deck comes from the request, never from the selection
///
/// For [`start_agent_action`]'s reason, and it is the same function doing it:
/// [`crate::dto::DeckScope::resolve`] matches the id against the observed set,
/// so a deck that left the fleet mid-flow is refused with that function's
/// error — which the dialog reads as "go back to the deck step" — and nothing
/// is ever asked of whichever deck happens to be selected.
///
/// A path gets the same string-shape check `desktop_resolve_project` applies
/// before spending a round trip on it; it touches no filesystem. The dialog
/// only sends paths a deck listed, so this is defence in depth (audit D2).
async fn list_directories_on(
    state: &DesktopState,
    deck_id: &str,
    path: Option<String>,
) -> Result<DesktopDirectoryListing, String> {
    if let Some(path) = path.as_deref() {
        validate_pasted_project_path(path)?;
    }
    let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    let answer = daemon
        .client
        .list_directories(path.as_deref())
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    Ok(match answer {
        GatedQuery::Answered(listing) => DesktopDirectoryListing::listing(
            listing.path,
            listing.parent,
            listing
                .entries
                .into_iter()
                .map(|entry| (entry.name, entry.path, entry.is_project)),
            listing.truncated,
        ),
        GatedQuery::Unsupported => DesktopDirectoryListing::Unsupported,
    })
}

/// [`desktop_new_agent_options`] minus the webview. Resolves its deck exactly
/// as [`list_directories_on`] does.
async fn new_agent_options_on(
    state: &DesktopState,
    deck_id: &str,
) -> Result<DesktopNewAgentOptions, String> {
    let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    let answer = daemon
        .client
        .new_agent_options()
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    let last_command = state.last_command(&scope.identity());
    Ok(match answer {
        GatedQuery::Answered(options) => DesktopNewAgentOptions::Deck {
            default_command: options.default_command,
            default_dir: options.default_dir,
            agents: options
                .agents
                .into_iter()
                .map(|agent| {
                    DesktopAgentOption::new(agent.id, &agent.display_name, agent.default_command)
                })
                .collect(),
            experimental: options.experimental,
            authoring_kinds: options.authoring_kinds,
            last_command,
        },
        GatedQuery::Unsupported => DesktopNewAgentOptions::Unsupported {
            desktop_agents: desktop_agent_registry(),
            last_command,
        },
    })
}

/// PRD #1223 M6: the orchestrations the New agent form can offer for `path` on
/// the deck `deck_id` names — that deck's `ResolveProject` answer.
///
/// `path` is the directory the form was opened on: one the deck listed, or one
/// the user typed. An ordinary directory is
/// [`DesktopNewAgentOrchestrations::NotProject`], not an error, because the
/// deck's refusal for it is the deliberately generic `unresolved` one; a deck
/// that cannot launch from this flow is
/// [`DesktopNewAgentOrchestrations::Unsupported`] with the reason.
#[tauri::command]
async fn desktop_new_agent_orchestrations(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: String,
    path: String,
) -> Result<DesktopNewAgentOrchestrations, String> {
    ensure_main_webview(&webview)?;
    new_agent_orchestrations_on(&state, &deck_id, &path).await
}

/// Why a deck cannot launch an orchestration from the New agent form, or
/// `None` when it can (PRD #1223 M6).
///
/// Two reasons, in this order. The deck lacks the project verbs, which the
/// connection already words as `projectActionsReason` — the sentence the Runs
/// screen shows. Or it lacks `prepared-role-command`, so it could not run a
/// role's configured command.
///
/// **A presentation read, not the gate.** It decides whether to offer the chips
/// and whether a launch is worth preparing at all — a preparation publishes the
/// coordinator context, which a deck that cannot then start the roles should
/// not be asked to write. What decides whether the flag is ever SENT is
/// [`DaemonClient::start_prepared_role`], from its own fresh handshake, so a
/// set captured here that has since gone stale can offer a chip without being
/// what decides the send.
async fn orchestration_launch_unavailable(
    daemon: &crate::daemon_bridge::TrustedDaemon,
) -> Result<Option<String>, String> {
    if let Some(reason) = daemon.connection().project_actions_reason {
        return Ok(Some(reason));
    }
    // Bounded (PRD #1223 audit F4): a handle whose cached set was invalidated
    // handshakes again here, and the launch that asks cannot be closed while
    // it waits.
    let capabilities = crate::daemon_bridge::bounded_reply(
        "the capability handshake",
        daemon.client.capabilities(),
    )
    .await?;
    Ok(
        (!capabilities.supports(dot_agent_deck::daemon_protocol::CAP_PREPARED_ROLE_COMMAND))
            .then(|| CONFIGURED_ROLE_COMMAND_UNSUPPORTED.to_string()),
    )
}

/// [`desktop_new_agent_orchestrations`] minus the webview. Resolves its deck
/// exactly as [`list_directories_on`] does, so a deck that left the fleet is
/// refused with `DeckScope::resolve`'s error and nothing is asked of any other.
async fn new_agent_orchestrations_on(
    state: &DesktopState,
    deck_id: &str,
    path: &str,
) -> Result<DesktopNewAgentOrchestrations, String> {
    validate_pasted_project_path(path)?;
    let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    if let Some(reason) = orchestration_launch_unavailable(&daemon).await? {
        return Ok(DesktopNewAgentOrchestrations::Unsupported { reason });
    }
    match daemon.client.resolve_project(path).await {
        Ok(project) => Ok(DesktopNewAgentOrchestrations::Project(
            map_resolved_project(project),
        )),
        // The resolve verb's one generic refusal: "not a project on this deck",
        // which is an answer here. Every other refusal is a real error.
        Err(ClientError::Server(message))
            if message.starts_with(&format!(
                "{}:",
                dot_agent_deck::daemon_protocol::PROJECT_ERR_UNRESOLVED
            )) =>
        {
            Ok(DesktopNewAgentOrchestrations::NotProject)
        }
        Err(error) => Err(safe_message(error.to_string())),
    }
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
    let snapshot = bootstrap(&options, &state.daemon).await;
    emit_snapshot(&app, &snapshot);
    ensure_snapshot_watchers(&app, &state);
    ensure_explicit_start_connected(options.start_if_missing, &snapshot)?;
    Ok(snapshot)
}

#[tauri::command]
// Eight, and the shape is the IPC boundary's rather than a design choice: Tauri
// deserialises a command's arguments from the webview's payload by NAME, so each
// wire field has to be a parameter. Grouping them into a struct would change the
// JSON the frontend sends, not the number of things being passed.
#[allow(clippy::too_many_arguments)]
async fn desktop_terminal_attach(
    app: AppHandle,
    webview: Webview,
    state: State<'_, DesktopState>,
    // PRD #1105 — the deck this agent is on, so the attach resolves THAT deck's
    // link through `DaemonLinks` instead of the process-global selected
    // endpoint. Agent ids are per-daemon monotonic integers, so without it an
    // attach for an agent on build-box streamed whatever `planner` the selected
    // deck happened to be running.
    //
    // Optional on the wire, and `None` means the selected deck — the behaviour
    // every caller had before this. The webview always names one; see
    // `terminal::endpoint_for_deck` for why the value is resolved against the
    // observed set rather than trusted as an address.
    deck_id: Option<String>,
    agent_id: String,
    on_output: Channel<Response>,
    // PRD #882 — the geometry this tile measured, declared so the agent is sized
    // to the smallest pane among every client watching it. Optional: a caller
    // with nothing measured yet (or the browser preview) declares nothing and
    // constrains nothing.
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<TerminalAttachResult, String> {
    ensure_main_webview(&webview)?;
    let viewport = match (rows, cols) {
        (Some(rows), Some(cols)) => Some((rows, cols)),
        // Both or neither: half a viewport is a caller bug, and guessing the
        // missing axis would register a constraint nobody asked for.
        _ => None,
    };
    terminal::attach(&app, &state, deck_id, agent_id, on_output, viewport).await
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

/// Which of the app's experimental surfaces to show (issue #1198) — the
/// desktop process's own flag, through one `features::show_desktop_*` wrapper
/// per surface. The webview asks once at startup; the flag itself is resolved
/// once, by [`init_features`], so restarting the app is how it changes.
#[tauri::command]
async fn desktop_features(webview: Webview) -> Result<dto::DesktopFeatures, String> {
    ensure_main_webview(&webview)?;
    Ok(dto::DesktopFeatures::current())
}

/// Resolve the desktop process's experimental flag (issue #1198). Until this
/// runs every `show_desktop_*` wrapper reads the default, OFF.
///
/// From this process's environment ONLY — `DOT_AGENT_DECK_EXPERIMENTAL`, and
/// the file `DOT_AGENT_DECK_FEATURES_CONFIG` names outright — through
/// `features::init_from_process_env`. There is deliberately no walk up from the
/// working directory for a `.dot-agent-deck.toml`, which is what the TUI and
/// the daemon do: that is a client-side project guess, exactly what PRD #819
/// removed from this crate and linkage-check rule 12 refuses here. A remote
/// deck's project is on another machine, and a Finder-launched app's working
/// directory is `/`. `docs/develop/experimental-flag.md` says how a packaged
/// app is given either variable.
fn init_features() {
    dot_agent_deck::features::init_from_process_env();
}

/// Read the desktop app's own settings document, and where it lives (PRD #803).
///
/// A standalone command rather than a `DesktopAction`, for the same reason the
/// terminal commands are: every `DesktopAction` ends in `refresh_and_emit`, so
/// routing a settings read through it would make reading a local TOML file cost
/// a `ListAgents` round-trip over the daemon socket — at launch, before the
/// daemon is necessarily up — and return the answer inside a snapshot that has
/// nowhere to put it. Settings are client-owned; nothing here touches the
/// daemon.
#[tauri::command]
async fn desktop_get_settings(
    webview: Webview,
) -> Result<settings::DesktopSettingsSnapshot, String> {
    ensure_main_webview(&webview)?;
    Ok(settings::load_snapshot())
}

/// Persist the desktop app's settings document and echo back what was written.
///
/// The whole document crosses the bridge, so the webview's read-modify-write is
/// one round trip and the file on disk is always a document this build's schema
/// produced.
///
/// # The reply is the input, not the disk
///
/// This echoes the document it was **given**, not the merged-and-reloaded state
/// on disk — nothing here re-reads the file. So a caller does not observe a
/// bumped `version`, a normalised value, or the unknown sections the merge
/// preserved until the next [`desktop_get_settings`]. Harmless for appearance,
/// where the input *is* the value the user chose; #741 and #802 must not build
/// on the echo reflecting what was written.
///
/// # The accepted strings are length-bounded
///
/// A compromised webview could otherwise send an arbitrarily long appearance
/// token that gets allocated and lowercased on the way in. The bound is
/// [`settings::MAX_APPEARANCE_TOKEN_BYTES`], checked inside `AppearanceMode`'s
/// deserializer *before* the normalising copy, so it covers this command and a
/// hand-edited document with one check; an over-length value fails argument
/// deserialisation with a message naming the limit and carrying no path.
///
/// A full strict DTO with `deny_unknown_fields` is deliberately **not** here.
/// It adds no new privilege class — a compromised main webview already holds
/// strictly stronger command and terminal surfaces than this one — while the
/// length bound is the part that removes the unbounded-allocation shape. What
/// remains outside our reach is the size of the IPC message itself, which the
/// framework parses before this signature is reached.
#[tauri::command]
async fn desktop_set_settings(
    app: AppHandle,
    webview: Webview,
    state: State<'_, DesktopState>,
    settings: DesktopSettings,
) -> Result<DesktopSettings, String> {
    ensure_main_webview(&webview)?;
    crate::settings::save(&settings).map_err(|error| {
        // The detail names the path and belongs in the app's own log; the
        // webview gets the sanitized half, the way connection errors already do.
        eprintln!("{}", error.detail());
        safe_message(error.public())
    })?;
    apply_selection(&app, &state, &settings).await;
    Ok(settings)
}

/// The three credential commands (PRD #802 M4), and the one that is missing.
///
/// # There is no `desktop_load_secret`, deliberately
///
/// The webview may ask whether a credential is stored, replace one and forget
/// one. It may **not** read one back, and that omission is the point of the
/// seam rather than an oversight: PRD #803's rule is that a secret goes in
/// neither `desktop.toml` nor `localStorage`, and a command returning a
/// credential to the webview would put it one `JSON.stringify` away from the
/// second half of that. `secrets::SecretStore::load` exists for the Rust side,
/// where M5's and M7's backends make their network call — which is where the
/// CSP already forces every network hop to happen, so nothing needs the value
/// over there.
///
/// # Each one answers with the resulting status
///
/// So a panel needs no second round trip to know what it is now looking at, and
/// cannot render a stale answer between the two.
///
/// # A failure is an `Err`, never a status saying "nothing stored"
///
/// The outcome PRD #802 M4 says to design against is *a user who thinks their
/// key is stored and finds voice broken tomorrow.* A store that failed
/// therefore rejects rather than resolving: the webview's `catch` is what shows
/// the sentence. `secrets::SecretStatus::problem` covers the other direction —
/// a *read* that could not find out, which is not the same as "nothing is
/// stored" and must not render as it.
///
/// # Blocking
///
/// A keychain call is a D-Bus round trip on Linux and can prompt on every
/// platform, so each one goes through `spawn_blocking` rather than sitting on
/// the async runtime the other commands share.
#[tauri::command]
async fn desktop_secret_status(webview: Webview, id: String) -> Result<SecretStatus, String> {
    ensure_main_webview(&webview)?;
    let id = parse_secret_id(&id)?;
    on_keychain(move |store| store.status(id)).await
}

#[tauri::command]
async fn desktop_store_secret(
    webview: Webview,
    id: String,
    secret: String,
) -> Result<SecretStatus, String> {
    ensure_main_webview(&webview)?;
    let id = parse_secret_id(&id)?;
    // The `Secret` is built here and moved into the closure, so the credential
    // exists as a plain `String` for exactly as long as it takes serde to hand
    // it over.
    let secret = Secret::new(secret);
    on_keychain(move |store| {
        store.store(id, &secret)?;
        Ok::<_, SecretError>(store.status(id))
    })
    .await?
    .map_err(report_secret_error)
}

#[tauri::command]
async fn desktop_forget_secret(webview: Webview, id: String) -> Result<SecretStatus, String> {
    ensure_main_webview(&webview)?;
    let id = parse_secret_id(&id)?;
    on_keychain(move |store| {
        store.delete(id)?;
        Ok::<_, SecretError>(store.status(id))
    })
    .await?
    .map_err(report_secret_error)
}

/// The webview names a credential by its token, and an unknown one is refused.
///
/// A closed set rather than a free string for `secrets::SecretId`'s reason: a
/// caller that could pass any name could strand a key under one nothing later
/// reads.
fn parse_secret_id(raw: &str) -> Result<SecretId, String> {
    SecretId::parse(raw).ok_or_else(|| {
        format!(
            "unknown credential id; the ids are {}",
            SecretId::ALL
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Run one blocking keychain operation off the async runtime.
///
/// The `Err` is the join failure — a panic inside the closure — and is separate
/// from whatever the operation itself returned, which is why the two commands
/// above have two `?`s.
async fn on_keychain<T: Send + 'static>(
    work: impl FnOnce(&KeychainSecretStore) -> T + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || work(&KeychainSecretStore::new()))
        .await
        .map_err(|error| safe_message(format!("the credential store call failed: {error}")))
}

/// A failed credential operation, split the way a failed settings save is: the
/// detail names whatever the platform said and belongs in the app's log, the
/// webview gets the sentence.
fn report_secret_error(error: SecretError) -> String {
    eprintln!("{}", error.detail());
    safe_message(error.public())
}

/// PRD #802 M7: the microphone, and the four commands the panel drives it with.
///
/// # What M6 calls, and in what order
///
/// `desktop_voice_status` to decide whether to offer a microphone at all;
/// `desktop_voice_start` on the first press; `desktop_voice_stop` on the
/// second, which closes the device, transcribes and answers with the
/// transcript; `desktop_voice_cancel` when the panel closes or the user escapes.
/// Between a start and a stop, `desktop_voice_status` is how the surface learns
/// that the length cap ended the recording on its own — from the user's side the
/// microphone simply stopped, and a panel that did not know why would go on
/// rendering *listening…* over a closed device.
///
/// # Stop transcribes; it does not hand back a buffer
///
/// The alternative shape — a third command that takes the audio and returns
/// text — would put a `Pcm16` on the IPC boundary, which is a base64 copy of up
/// to 960 KB of the user's voice crossing into the webview for no reason. PRD
/// #802's Open Question 5 says no part of an utterance is persisted; keeping
/// the audio inside this process is the same rule applied one seam earlier. The
/// webview receives a transcript, which it has to render anyway, and nothing
/// else.
///
/// # There is no `desktop_voice_transcribe` taking a credential either
///
/// The key is read Rust-side from the keychain at call time, exactly as PRD
/// #802 M4 and M5 established. `desktop_secret_status`, `…_store` and
/// `…_forget` remain the whole credential surface, and `load` is still
/// deliberately absent.
pub(crate) struct VoiceState {
    /// One session per process: one microphone, one utterance at a time — and,
    /// since PRD #802's sleep work, the machine held awake beside it.
    ///
    /// A [`voice::VoiceHold`] rather than the session on its own, and the
    /// grouping is load-bearing rather than tidiness: it owns both resources
    /// voice holds on the user's machine and is the only thing that can end
    /// either. Its module docs carry the property and how the compiler holds
    /// it.
    ///
    /// Cheap to clone — both halves are `Arc`s — which is what the cap timer
    /// (a spawned task outliving the command that started it) and every
    /// `spawn_blocking` below need.
    hold: voice::VoiceHold,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            // Constructs no host and opens no device — `cpal` is not touched
            // until a `start`. An app on a machine with no audio server starts
            // normally and finds out at the first press, which is where the
            // sentence for it already is. The sleep inhibit is the same: the
            // platform is not asked for anything until voice is switched on.
            hold: voice::VoiceHold::new(Arc::new(voice::CpalSource::new())),
        }
    }
}

/// What the webview is told about the microphone.
///
/// [`voice::CaptureStatus`]'s four fields, flattened, plus the two that come
/// from the settings document rather than from the session — which is why this
/// lives here and not in `voice::capture`, a module that deliberately reads no
/// settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceStatus {
    #[serde(flatten)]
    pub capture: voice::CaptureStatus,
    /// Whether a microphone path is offered at all.
    ///
    /// **Always true from this process since `Speech = off` went**, and that is
    /// worth stating rather than leaving to be rediscovered: every speech
    /// backend this build ships can run, so no settings document can turn the
    /// stage off. What used to be reported here — *nothing is set up* — is now
    /// reported where it actually happens, as
    /// [`voice::TranscriptionOutcome::NotConfigured`] naming the container to
    /// start or the key to paste.
    ///
    /// The field stays because the SURFACE still has the question and two other
    /// answerers: a runtime with no capture verbs at all, and the browser
    /// fixture, which has no Rust side and therefore nothing to transcribe
    /// with. Both report `false` and both render `VOICE_UNAVAILABLE`.
    pub available: bool,
    /// Which transcriber would answer — `local` or `remote`.
    pub backend: &'static str,
}

/// The transcription backend the settings document currently names.
///
/// Read per call rather than cached, for `voice::resolver_for`'s reason: a user
/// who changes the setting uses it on the next utterance instead of after a
/// restart. **That is what makes pressing Voice again after choosing a backend
/// work without a restart**, which PRD #802 M6's rewrite turned from a nicety
/// into the documented behaviour of the unavailable path.
///
/// **The cost is no longer "one small TOML read per button press".** It was,
/// while the only status reads were one per press plus one a second inside an
/// open dialog. Continuous voice control polls `desktop_voice_status` every
/// `VOICE_STATUS_POLL_MS` — 250 ms — for as long as the microphone is open, so
/// this is **four small TOML reads a second** in that state and one per call
/// everywhere else. At a few tens of microseconds each that is not worth
/// caching away the freshness above.
///
/// One consequence is worth naming rather than discovering: `load_snapshot`
/// logs a malformed document through `log_document_problem`, which is a bare
/// `eprintln!` with no rate limit. A `desktop.toml` this build cannot parse
/// therefore writes four stderr lines a second while voice is on, where it
/// wrote one. It is a misconfiguration either way, and the fix — if it ever
/// matters — is a log-once latch in `settings.rs` rather than a cache here.
fn voice_speech_settings() -> crate::settings::TranscriptionSettings {
    crate::settings::load_snapshot()
        .settings
        .voice
        .unwrap_or_default()
        .transcription
}

fn voice_status(hold: &voice::VoiceHold) -> VoiceStatus {
    VoiceStatus {
        capture: hold.status(),
        // See the field's doc comment: no settings document can turn the stage
        // off any more, and the not-set-up case is reported at the moment it
        // bites rather than as a permanent state of the app.
        available: true,
        backend: voice_speech_settings().backend.as_token(),
    }
}

/// A refused or failed capture, split the way a failed credential operation is:
/// the detail belongs in the app's own log and the webview gets the sentence.
///
/// Nothing here prints a transcript or a sample — a capture error names the
/// device or the state machine and never what was said, which is the rule PRD
/// #802's Open Question 5 sets and this is the one place it could be broken by
/// accident.
fn report_capture_error(error: voice::CaptureError) -> String {
    safe_message(error.detail())
}

/// Open the microphone. Idle, done or failed → recording.
#[tauri::command]
async fn desktop_voice_start(
    webview: Webview,
    voice_state: State<'_, VoiceState>,
) -> Result<VoiceStatus, String> {
    ensure_main_webview(&webview)?;
    let hold = voice_state.hold.clone();

    // Opening an audio device is a round trip to the OS and can prompt, so it
    // goes through `spawn_blocking` rather than sitting on the async runtime
    // every other command shares — the same treatment a keychain call gets.
    //
    // PRD #802's sleep work rides in the same closure for the same reason:
    // [`voice::VoiceHold::start`] asks the platform to keep the machine awake
    // after the device opens, which on Linux is a D-Bus round trip. Both are
    // blocking calls to the operating system, so both belong off the runtime.
    let started = {
        let hold = hold.clone();
        tauri::async_runtime::spawn_blocking(move || hold.start())
            .await
            .map_err(|error| safe_message(format!("the microphone call failed: {error}")))?
    };
    let (_, ticket) = started.map_err(report_capture_error)?;

    // The other half of the length cap. `voice::PcmSink` already refuses to
    // grow past `MAX_UTTERANCE`, which bounds the allocation; this is what
    // releases the DEVICE, so a forgotten toggle does not leave a microphone
    // open for the life of the app. The ticket is what stops a timer outliving
    // its own utterance and closing the next one.
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(voice::MAX_UTTERANCE).await;
        // Blocking for the reason the open above is: releasing the device
        // joins its thread, which is as slow as the platform's teardown.
        let _ = tauri::async_runtime::spawn_blocking(move || hold.cap_reached(ticket)).await;
    });

    Ok(voice_status(&voice_state.hold))
}

/// Close the microphone and transcribe what it heard. Recording →
/// transcribing → done or failed.
#[tauri::command]
async fn desktop_voice_stop(
    webview: Webview,
    voice_state: State<'_, VoiceState>,
) -> Result<voice::VoiceTranscription, String> {
    ensure_main_webview(&webview)?;
    // Blocking, like the open in `desktop_voice_start`: closing the device
    // joins its thread, and a driver that is slow to let go would otherwise
    // park a runtime worker for as long as it takes.
    let hold = voice_state.hold.clone();
    let audio = tauri::async_runtime::spawn_blocking(move || hold.stop())
        .await
        .map_err(|error| safe_message(format!("the microphone call failed: {error}")))?
        .map_err(report_capture_error)?;
    let transcriber = voice::transcriber_for(
        &voice_speech_settings(),
        Arc::new(KeychainSecretStore::new()),
    );
    let result = voice::handle_audio(transcriber.as_ref(), &audio).await;
    voice_state.hold.settle(result.outcome.is_heard());
    Ok(result)
}

/// What the microphone is doing right now, and whether one is offered at all.
#[tauri::command]
async fn desktop_voice_status(
    webview: Webview,
    voice_state: State<'_, VoiceState>,
) -> Result<VoiceStatus, String> {
    ensure_main_webview(&webview)?;
    Ok(voice_status(&voice_state.hold))
}

/// Abandon the recording without transcribing it — a closed panel, an escape
/// key, a user who changed their mind.
///
/// Idempotent and never refused, because each of those can arrive in any state
/// and a caller that has to know which one it is in would get it wrong.
#[tauri::command]
async fn desktop_voice_cancel(
    webview: Webview,
    voice_state: State<'_, VoiceState>,
) -> Result<VoiceStatus, String> {
    ensure_main_webview(&webview)?;
    // Blocking for `desktop_voice_stop`'s reason — a cancel releases the same
    // device, and a closing panel is exactly when several of these arrive at
    // once.
    //
    // `release` rather than a bare cancel, and it is the ONLY spelling
    // available: `VoiceHold` keeps the session private precisely so this call
    // cannot close the microphone without also letting the machine sleep. This
    // is the press that turns voice off, so it is the one that must.
    let hold = voice_state.hold.clone();
    tauri::async_runtime::spawn_blocking(move || hold.release())
        .await
        .map_err(|error| safe_message(format!("the microphone call failed: {error}")))?;
    Ok(voice_status(&voice_state.hold))
}

/// The longest utterance this build will resolve, in bytes.
///
/// The panel's own transcripts are bounded — `voice::MAX_UTTERANCE` caps the
/// audio at 30 seconds — but that cap bounds the *audio*, not what arrives at
/// the IPC boundary, and this command trusts that boundary no more than
/// [`validate_agent_id`] does: an utterance becomes part of a model prompt. It
/// was also an argument to a child process while the agent-CLI intent backend
/// existed, and the bound outlived that backend because the first reason is
/// enough on its own. 2 KiB is far past any spoken command — thirty seconds of
/// speech is around 700 characters — while making a payload-shaped value
/// impossible.
const MAX_UTTERANCE_BYTES: usize = 2 * 1024;

/// The bounds on what the webview may declare its directory browser to be
/// showing (PRD #1223), checked by [`validate_voice_directories`] before any of
/// it reaches a model prompt or a resolver.
///
/// The entry count is the deck's own listing cap (`MAX_DIRECTORY_ENTRIES` in the
/// root crate's `directory_listing`, 1,000): a real declaration is a filtered
/// subset of one listing, so it can never exceed that. Written out rather than
/// imported, because this crate reaches into no root module about the deck's
/// filesystem (linkage-check rule 12, PRD #819); a deck that raised its cap would
/// make a full listing refused here, which fails loudly rather than silently.
/// The byte bounds are wide of any real filesystem — a component is 255 bytes on
/// every filesystem this app ships to, a path at most `PATH_MAX` — and narrow of
/// a payload.
const MAX_VOICE_DIRECTORY_ENTRIES: usize = 1_000;
const MAX_VOICE_DIRECTORY_NAME_BYTES: usize = 1024;
const MAX_VOICE_DIRECTORY_PATH_BYTES: usize = 4096;
const MAX_VOICE_DECK_ID_BYTES: usize = 256;

/// Refuse a directory declaration no real browser could have produced.
///
/// A refusal is an `Err` for the whole command rather than a declaration
/// quietly dropped: dropping it would make the directory rows `callable: false`
/// and render a hint telling the user to open a dialog that IS open, which is a
/// wrong sentence rather than an honest failure. Only a misbehaving page can
/// reach it.
fn validate_voice_directories(directories: &voice::VoiceDirectories) -> Result<(), String> {
    let too_long = |value: &str, limit: usize| value.len() > limit;
    if directories.entries.len() > MAX_VOICE_DIRECTORY_ENTRIES
        || too_long(&directories.deck_id, MAX_VOICE_DECK_ID_BYTES)
        || too_long(&directories.path, MAX_VOICE_DIRECTORY_PATH_BYTES)
        || directories.entries.iter().any(|entry| {
            too_long(&entry.name, MAX_VOICE_DIRECTORY_NAME_BYTES)
                || too_long(&entry.path, MAX_VOICE_DIRECTORY_PATH_BYTES)
        })
    {
        return Err(
            "the directory listing sent with that command is larger than any deck lists"
                .to_string(),
        );
    }
    Ok(())
}

/// The bounds on the New agent form a webview may declare (PRD #1223), checked
/// by [`validate_voice_new_agent`] for [`validate_voice_directories`]' reason.
///
/// Both lists are small closed sets on a real form: the Mode row is `No mode`,
/// one chip per orchestration a project defines, and three authoring kinds; the
/// agent list is a deck's registry. The caps are far above either
/// and far below a payload.
const MAX_VOICE_FORM_CHOICES: usize = 256;
const MAX_VOICE_FORM_CHOICE_BYTES: usize = 1024;

/// Refuse a New agent declaration no real dialog could have produced.
fn validate_voice_new_agent(new_agent: &voice::VoiceNewAgent) -> Result<(), String> {
    let Some(form) = &new_agent.form else {
        return Ok(());
    };
    let too_long = |value: &str, limit: usize| value.len() > limit;
    let oversized = |choices: &[voice::VoiceChoice]| {
        choices.len() > MAX_VOICE_FORM_CHOICES
            || choices.iter().any(|choice| {
                too_long(&choice.id, MAX_VOICE_FORM_CHOICE_BYTES)
                    || too_long(&choice.label, MAX_VOICE_FORM_CHOICE_BYTES)
            })
    };
    if too_long(&form.deck_id, MAX_VOICE_DECK_ID_BYTES)
        || too_long(&form.path, MAX_VOICE_DIRECTORY_PATH_BYTES)
        || oversized(&form.modes)
        || oversized(&form.agent_types)
        || oversized(&form.withheld_modes)
    {
        return Err(
            "the New agent form sent with that command is larger than any form shows".to_string(),
        );
    }
    Ok(())
}

/// The bounds on the deck step a webview may declare (PRD #1223), checked by
/// [`validate_voice_deck_step`] for [`validate_voice_directories`]' reason.
///
/// A deck step lists the observed fleet, a handful of decks; the reason is
/// display text the webview has already cut to `DISPLAY_LIMITS.message` (240
/// characters, so at most 960 bytes).
const MAX_VOICE_DECK_STEP_ROWS: usize = 256;
const MAX_VOICE_DECK_REASON_BYTES: usize = 1024;

/// Refuse a deck step no real dialog could have produced.
fn validate_voice_deck_step(deck_step: &[voice::VoiceDeckChoice]) -> Result<(), String> {
    if deck_step.len() > MAX_VOICE_DECK_STEP_ROWS
        || deck_step.iter().any(|choice| {
            choice.deck_id.len() > MAX_VOICE_DECK_ID_BYTES
                || choice
                    .reason
                    .as_ref()
                    .is_some_and(|reason| reason.len() > MAX_VOICE_DECK_REASON_BYTES)
        })
    {
        return Err(
            "the deck list sent with that command is larger than any fleet shows".to_string(),
        );
    }
    Ok(())
}

/// The bound on the Deck selector section a webview may send with an utterance
/// (PRD #1195), checked by [`validate_voice_endpoints`] for
/// [`validate_voice_directories`]' reason. Each row's fields are already
/// bounded by the settings schema's own types as they deserialize; this bounds
/// how many there are. A selector lists a handful of decks.
const MAX_VOICE_SELECTOR_ROWS: usize = 256;

/// Refuse a Deck selector section no real selector could have rendered.
fn validate_voice_endpoints(endpoints: &crate::settings::EndpointSettings) -> Result<(), String> {
    if endpoints.remote.len() > MAX_VOICE_SELECTOR_ROWS {
        return Err(
            "the deck list sent with that command is larger than any Deck selector shows"
                .to_string(),
        );
    }
    Ok(())
}

/// PRD #802 M6: take one utterance to an outcome carrying the sentence to show.
///
/// # What it does NOT do
///
/// It runs nothing. A [`voice::VoiceOutcome::Dispatch`] names an entry in the
/// frontend action registry (`desktop/src/lib/voiceActions.ts`) and the webview
/// dispatches it where a click dispatches one — so this command's whole output
/// is a classified situation plus the sentence for it, and every refusal is one
/// of those situations rather than an `Err`. The `Err`s below are the two things
/// that are not situations: a call that came from somewhere it must not, and an
/// utterance the boundary refuses to carry.
///
/// # The screen is a parameter and the agents are not
///
/// An utterance is resolved against the live state the app already holds, and
/// each piece of that state is read where it already lives. The agent list is
/// read HERE, from the selected deck's own snapshot, because that is where it
/// lives and a list arriving from the webview would be a second answer to "what
/// agents are there" with nothing keeping the two in step. The mounted screen
/// cannot be read here at all — it is React state — so it is the one piece the
/// webview states, which is what `DeckBridge.declareVoiceScreen` is.
///
/// **`directories` is the second such piece** (PRD #1223): what the New agent
/// dialog's directory browser is showing, which is that component's state and
/// nobody else's — the daemon lists one level per request and keeps none of
/// them. It is declared in the same call, for the same reason, and bounded by
/// [`validate_voice_directories`]. It is an IPC argument between this app's own
/// webview and its own Rust half; nothing about it reaches the daemon.
///
/// **`new_agent` is the third** (PRD #1223): the New agent form's Mode chips
/// and agent entries as they are on screen, present while the dialog is
/// open. Same route, same reason, bounded by [`validate_voice_new_agent`], and
/// likewise never sent to the daemon.
///
/// **`deck_step` is the fourth**: the New agent dialog's deck step — every
/// deck it lists and why each one it disables cannot take a spawn — declared
/// on every utterance, because the row it matters to opens the dialog. It
/// only annotates the decks read here ([`voice_decks`]), bounded by
/// [`validate_voice_deck_step`], and never reaches the daemon either.
///
/// **`endpoints` is the fifth** (PRD #1195): the `[endpoints]` section the Deck
/// selector is rendering, which is `useDesktopSettings`' React state. That
/// state is applied the moment the user edits it and written to disk behind
/// it, so reading `desktop.toml` here instead would refuse "switch deck to"
/// a deck the selector already shows (Qodo on PR #1340) — and would keep
/// refusing it if that write failed, since the edit stays applied on screen.
/// It crosses as the settings schema's own [`crate::settings::EndpointSettings`],
/// so every row is held to the same field types a saved document is, and
/// [`validate_voice_endpoints`] bounds the row count. It is trusted no further
/// than that: it only decides which decks a spoken name can resolve to and
/// which selector token each maps to, and the webview's
/// `chooseDeckSelection` re-checks the token and the row's address against
/// its current settings before writing any switch. Like the other four it is
/// an IPC argument between this app's own webview and its own Rust half, in
/// one binary, and never reaches the daemon.
///
/// # One `ListAgents` per utterance
///
/// [`get_snapshot`] fetches rather than reading a cache, which is one daemon
/// round trip per voice command. That is deliberate and it is cheap next to what
/// follows it: the intent backend measured **0.62–1.03 s** for the call this
/// snapshot is gathered for — and 4.30–6.30 s while the agent-CLI backend was
/// the default, which is the number PRD #802's risk entry was written against.
/// Resolving "open the tester" against a list from a minute ago is how a spoken
/// name resolves to an agent that has since exited.
///
/// The SELECTED deck's agents, not the fleet's. Every other single-deck command
/// reads the same snapshot, `open_agent` dispatches a pane whose terminal is the
/// selected deck's, and a fleet-wide list would let a spoken name resolve to an
/// agent on a machine the user is not looking at.
#[tauri::command]
// Eight, for `desktop_terminal_attach`'s reason: Tauri deserialises each wire
// field by NAME, so each declaration piece has to be a parameter.
#[allow(clippy::too_many_arguments)]
async fn desktop_voice_resolve(
    webview: Webview,
    state: State<'_, DesktopState>,
    utterance: String,
    screen: voice::Screen,
    directories: Option<voice::VoiceDirectories>,
    new_agent: Option<voice::VoiceNewAgent>,
    deck_step: Option<Vec<voice::VoiceDeckChoice>>,
    endpoints: Option<crate::settings::EndpointSettings>,
) -> Result<voice::VoiceResult, String> {
    ensure_main_webview(&webview)?;
    if utterance.len() > MAX_UTTERANCE_BYTES {
        return Err(format!(
            "that command is too long to send — {MAX_UTTERANCE_BYTES} bytes at most"
        ));
    }
    if let Some(directories) = &directories {
        validate_voice_directories(directories)?;
    }
    if let Some(new_agent) = &new_agent {
        validate_voice_new_agent(new_agent)?;
    }
    if let Some(deck_step) = &deck_step {
        validate_voice_deck_step(deck_step)?;
    }
    if let Some(endpoints) = &endpoints {
        validate_voice_endpoints(endpoints)?;
    }
    // Read per call rather than cached, for `voice_speech_settings`'s reason: a
    // user who changes the backend, the endpoint or the model uses it on the
    // next utterance instead of after a restart.
    let settings = crate::settings::load_snapshot()
        .settings
        .voice
        .unwrap_or_default();
    let resolver = voice::resolver_for(&settings.intent, Arc::new(KeychainSecretStore::new()));
    let snapshot = get_snapshot(&state.daemon).await;
    let mut decks = voice_decks(&snapshot.observed, deck_step.as_deref());
    // PRD #1195 M3: the decks the Deck selector lists, as the webview sent
    // them — the section the selector is rendering, not `desktop.toml`, which
    // lags it by a queued write — rather than only the ones the app observes,
    // which under a single-deck selection is the one deck already shown.
    let selections = selector_voice_decks(endpoints.as_ref(), &mut decks, deck_step.as_deref());
    let mut result = voice::handle_utterance_with(
        resolver.as_ref(),
        voice::table(),
        screen,
        &snapshot.agents,
        &decks,
        directories.as_ref(),
        new_agent.as_ref(),
        voice::Transcript::new(utterance),
        settings.labels,
        // Issue #1198: the deck is an experimental surface, so voice neither
        // offers nor dispatches the way there while it is hidden.
        dot_agent_deck::features::show_desktop_deck(),
    )
    .await;
    voice::address_deck_switch(&mut result.outcome, |deck_id| {
        selections.get(deck_id).cloned()
    });
    Ok(result)
}

/// PRD #1195 M3 — the Deck selector's decks, for `switch_deck`: every deck it
/// lists that [`voice_decks`] did not already take from the observed fleet is
/// appended to `decks`, and the answer maps EVERY deck in `decks` that the
/// selector lists to the token the selector stores for it — and, for a remote
/// row, the address it had when read ([`voice::VoiceDeckIdentity`]), which the
/// webview compares with the row before it writes the switch.
///
/// # Why the observed fleet is not enough
///
/// [`voice_decks`] reads `snapshot.observed`, which is what the app CONNECTS
/// to — and under a single-deck selection that is exactly one deck, the one on
/// screen (`EndpointSettings::connectable_endpoints`). Resolving a switch
/// against it would leave "switch deck to the build box" one answer, "no deck
/// matches", for every deck but the current one. The selector lists `local`
/// and every `[[endpoints.remote]]` row (`deckChoices` in
/// `desktop/src/lib/endpoints.ts`), so that is the list read here, from the
/// section the webview's selector is rendering, sent with the utterance (see
/// [`desktop_voice_resolve`]'s `endpoints`). `None` — a webview that sent none —
/// lists the local deck alone.
///
/// # Keys and labels
///
/// Each appended deck is keyed the way the fleet would key it — the endpoint's
/// wire id, or [`crate::dto::unconfigured_deck_id`] for a row with no socket
/// path — so a deck that later connects keeps its key, and labelled the way the
/// overview labels it. It is appended as unable to take a new agent, since the
/// New agent dialog lists only decks the app is connected to: with the deck
/// step's own reason when the step names one, otherwise
/// [`voice::DECK_NOT_CONNECTED`], or the fleet view's "not configured"
/// sentence for a row with no address. That keeps the New agent flow exactly
/// as it was — it never offers or preselects such a deck — while
/// `switch_deck`, which ignores that reason, can switch to it.
///
/// **All Decks is not in here.** It is a selection rather than a deck, so a
/// `deck_ref` naming it would also be a deck the New agent dialog is asked
/// about; see `commands.toml`'s `switch_deck` row.
fn selector_voice_decks(
    endpoints: Option<&crate::settings::EndpointSettings>,
    decks: &mut Vec<voice::VoiceDeck>,
    deck_step: Option<&[voice::VoiceDeckChoice]>,
) -> HashMap<String, voice::VoiceDeckSelection> {
    use dot_agent_deck::daemon_client::Endpoint;
    let step_reason = |deck_id: &str| {
        deck_step
            .and_then(|step| step.iter().find(|choice| choice.deck_id == deck_id))
            .and_then(|choice| choice.reason.clone())
    };
    let local = crate::dto::deck_wire_id(&Endpoint::local());
    // The local deck carries no identity: it has no remote address that
    // Settings can change under its token.
    let mut listed: Vec<(voice::VoiceDeck, voice::VoiceDeckSelection)> = vec![(
        voice::VoiceDeck {
            unavailable: Some(
                step_reason(&local).unwrap_or_else(|| voice::DECK_NOT_CONNECTED.to_string()),
            ),
            id: local,
            label: "Local deck".to_string(),
            local: true,
        },
        voice::VoiceDeckSelection {
            token: crate::settings::LOCAL_SELECTION_TOKEN.to_string(),
            identity: None,
        },
    )];
    for row in endpoints
        .map(|section| section.remote.as_slice())
        .unwrap_or_default()
    {
        let (id, label, fallback) = match row.endpoint() {
            Some(remote) => {
                let endpoint = Endpoint::Remote(remote);
                (
                    crate::dto::deck_wire_id(&endpoint),
                    crate::dto::deck_path_text(&endpoint),
                    voice::DECK_NOT_CONNECTED,
                )
            }
            None => (
                crate::dto::unconfigured_deck_id(&row.id),
                crate::dto::safe_display_text(row.describe()),
                crate::dto::UNCONFIGURED_DECK_REASON,
            ),
        };
        let unavailable = Some(step_reason(&id).unwrap_or_else(|| fallback.to_string()));
        listed.push((
            voice::VoiceDeck {
                label: if label.trim().is_empty() {
                    "Remote deck".to_string()
                } else {
                    label
                },
                id,
                local: false,
                unavailable,
            },
            voice::VoiceDeckSelection {
                token: row.id.as_str().to_string(),
                identity: Some(voice::VoiceDeckIdentity {
                    host: row.host.as_str().to_string(),
                    user: row.user.as_ref().map(|user| user.as_str().to_string()),
                    port: row.port.get(),
                    socket: row
                        .socket
                        .as_ref()
                        .map(|socket| socket.as_str().to_string()),
                    identity: row
                        .identity
                        .as_ref()
                        .map(|identity| identity.as_str().to_string()),
                    jump: row.jump.as_ref().map(|jump| jump.as_str().to_string()),
                }),
            },
        ));
    }
    let mut selections = HashMap::new();
    for (deck, selection) in listed {
        if !decks.iter().any(|known| known.id == deck.id) {
            decks.push(deck.clone());
        }
        selections.entry(deck.id).or_insert(selection);
    }
    selections
}

/// The decks a spoken `deck_ref` resolves against (PRD #1223): the snapshot's
/// own `observed` list — every deck the app connects to, named the way the
/// overview names it — rather than anything the webview sends.
///
/// **The whole fleet, unlike the agents above**, which are the selected deck's.
/// An agent reference means "one I can see", so it stays on the deck in view; a
/// deck reference exists to name a deck OTHER than the one in view.
///
/// The label is `deckName`'s (`desktop/src/lib/displayText.ts`): "Local deck"
/// for the local endpoint, the `user@host[:port]` label for a remote one — so a
/// report or an ambiguity sentence names a deck the way the screen does.
///
/// **Eligibility is the webview's `deck_step`**, the New agent dialog's deck
/// step as it stands ([`voice::VoiceDeckChoice`] says why that one piece is
/// declared): a deck it gives a reason keeps that reason, word for word, and a
/// deck it does not list at all is one the webview's fleet has not heard from
/// ([`voice::DECK_NOT_REPORTED`]). With no declaration every deck is taken as
/// eligible, which is what voice assumed before it was told.
fn voice_decks(
    observed: &[crate::dto::ObservedDeckDto],
    deck_step: Option<&[voice::VoiceDeckChoice]>,
) -> Vec<voice::VoiceDeck> {
    observed
        .iter()
        .map(|deck| {
            let local = deck.deck_kind != "remote";
            let unavailable = deck_step.and_then(|step| {
                match step.iter().find(|choice| choice.deck_id == deck.deck_id) {
                    Some(choice) => choice.reason.clone(),
                    None => Some(voice::DECK_NOT_REPORTED.to_string()),
                }
            });
            voice::VoiceDeck {
                id: deck.deck_id.clone(),
                label: if local || deck.label.trim().is_empty() {
                    if local { "Local deck" } else { "Remote deck" }.to_string()
                } else {
                    deck.label.clone()
                },
                local,
                unavailable,
            }
        })
        .collect()
}

/// PRD #802 — what can be said on this screen, for the discovery overlay.
///
/// # It is the TABLE, annotated, and deliberately the same shape the model gets
///
/// The overlay's requirement is that it be *generated from the table, never a
/// maintained list*, so this returns exactly what [`voice::annotate`] hands the
/// intent backend: each row's `id`, its `description` and whether the current
/// screen can run it. Handing the webview a second, prettier projection would
/// be the maintained list under a better name — and the first time a row's
/// wording changed, the overlay and the model would be telling the user and the
/// model two different things.
///
/// **So the `description` a user reads here is a PROMPT**, written for a model
/// and reviewed as an interface (`commands.toml` says so at the column). That
/// is a real cost and it is the deliberate side of the trade: a separate
/// user-facing column would read better and would be a second wording to keep
/// in step, which is the whole defect class this table exists to close.
///
/// # No daemon round trip, no model, no state
///
/// Unlike [`desktop_voice_resolve`] this reaches nothing: the table is
/// `include_str!`d into the binary and the screen arrives as a parameter, so
/// the answer is a pure function of the two. It costs no `ListAgents`, spends
/// no credential, and is safe to call every time the overlay opens rather than
/// being cached into something that can go stale.
#[tauri::command]
async fn desktop_voice_commands(
    webview: Webview,
    screen: voice::Screen,
    directories: Option<voice::VoiceDirectories>,
    new_agent: Option<voice::VoiceNewAgent>,
) -> Result<Vec<voice::AnnotatedCommand>, String> {
    ensure_main_webview(&webview)?;
    if let Some(directories) = &directories {
        validate_voice_directories(directories)?;
    }
    if let Some(new_agent) = &new_agent {
        validate_voice_new_agent(new_agent)?;
    }
    Ok(voice::annotate_for(
        voice::table(),
        screen,
        directories.as_ref(),
        new_agent.as_ref(),
        // Read per call, for `desktop_voice_resolve`'s reason: the overlay says
        // what the NEXT utterance can do, so it follows the label choice too.
        crate::settings::load_snapshot()
            .settings
            .voice
            .unwrap_or_default()
            .labels,
        // Issue #1198: the list marks the deck's row unavailable while the deck
        // is hidden, for the same reason the resolver refuses it.
        dot_agent_deck::features::show_desktop_deck(),
    ))
}

/// Put a saved document's deck selection into force (PRD #741 M7, completed at
/// M9).
///
/// # Every settings save reaches here, and most of them changed no deck
///
/// The command that calls this is `desktop_set_settings`, which is also how a
/// theme, a zoom level and every future preference are written. So the work
/// below is split in two by [`selection_moved`], and getting that split wrong is
/// not a matter of efficiency: the switch half **detaches every terminal
/// session**, and running it unconditionally would tear down a user's live
/// panes because they changed the colour scheme.
///
/// # Always, because a save is rare and dropping a held classification is cheap
///
/// 1. **The selection is applied**, so `selected_endpoint()` — and therefore the
///    snapshot, the banner and the Stop/Replace gating — name the deck the
///    document now says.
/// 2. **The held handshake is dropped.** A classification describes one daemon;
///    after any save it may describe the wrong one, and holding it would report
///    the old deck's agent count beside the new deck's name for up to
///    `HANDSHAKE_REVALIDATE_INTERVAL`.
/// 3. **Every transport except the observed decks' is released** — rule 3 of
///    `endpoint_tunnels`, and the leak PRD #741 M7 names explicitly: without it
///    each selection change leaves an authenticated `ssh -N -L` child behind for
///    the life of the app. A lease already handed out survives this, so nothing
///    in flight is torn out from under — which since M9 includes a terminal
///    session's own lease, not merely the link's. **Observed decks**, plural,
///    since PRD #742 M2: under `Selection::All` that is every configured deck
///    with somewhere to connect to, and under any single-deck selection it is
///    the one deck `resolve()` names, which is what this line meant before.
///
/// # Only when the deck actually moved
///
/// 4. **Every terminal session is detached.** A session streams from ONE
///    daemon's PTY; after a selection change every one of them is showing the
///    deck the user has left. The same pairing `StopDaemon` and `RestartDaemon`
///    already make, and for the same reason: a tile left attached to a deck that
///    is no longer selected is a tile whose keystrokes go to another machine's
///    agent.
/// 5. **The watcher is told** (M9). Its event subscription is a connection to
///    one daemon and a selection change does not end it, so without this it
///    would keep folding the old deck's broadcasts into the view that answers
///    the new deck's snapshots. See [`DesktopState::selection`].
/// 6. **A snapshot for the new deck is emitted** (M9). The watcher re-subscribes
///    within a moment and would emit one of its own on its next event or
///    reconcile tick, but "within five seconds" is not an answer to a click.
///    This is also the step that must not run on an ordinary save: against an
///    unreachable remote deck it costs a full connect timeout, and putting that
///    in front of a theme change would make the whole settings sheet feel stuck.
///
/// The order matters in two places: the sessions are detached before the tunnels
/// are released, so a detach frame still has a transport to travel over; and the
/// links are dropped before the tunnels, so a link cannot be re-established
/// against a transport that is on its way out.
async fn apply_selection(app: &AppHandle, state: &DesktopState, settings: &DesktopSettings) {
    let moved = retarget_selection(state, settings).await;
    // PRD #742 M3: OUTSIDE the `moved` gate, and that is the point of putting it
    // here rather than inside `retarget_selection`. Watchers follow the OBSERVED
    // SET, and adding a deck to a fleet grows that set without moving the deck
    // the screen resolves to — so gating this on `moved` would leave a newly
    // added deck permanently unwatched. `retarget_selection` has already ended
    // the departed decks' watchers; this starts the arrived ones', and does
    // nothing at all for a save that changed neither.
    ensure_snapshot_watchers(app, state);
    if !moved {
        return;
    }
    refresh_and_emit(app, &state.daemon).await;
}

/// Stop one agent on the selected deck, and tear down **that deck's** terminal
/// session for it.
///
/// # ONE capture, and why this used to be two reads
///
/// This read `selected_endpoint()` twice: once through `trusted_daemon`, before
/// `DaemonLinks::trusted`, and again *after* `stop_agent(…).await` returned, to
/// decide which deck's session to detach. The first read pinned the stop to
/// deck A correctly. The second was a fresh question about a mutable global,
/// asked on the far side of a daemon round trip — so moving the selection while
/// the request was in flight stopped A's `planner` and detached **B's** same-id
/// session, closing a terminal on a machine this action never touched. Agent
/// ids are per-daemon monotonic integers, so the collision is the ordinary case
/// rather than a contrived one; editing the selected deck's *address* mid-stop
/// has the same shape.
///
/// The comment that used to sit at the detach asserted the two reads named the
/// same deck. They are separated by asynchronous daemon work and nothing holds
/// the selection still, so it was simply wrong.
///
/// [`crate::dto::DeckScope`] is the fix and the general form of it: capture the
/// deck once, before the first await, and let every later step — cleanup very
/// much included — read only the captured value.
///
/// # Split out for the same reason [`retarget_selection`] is
///
/// Everything here is testable and the snapshot emit around it is not. That is
/// what lets a test drive the *caller* against a scripted daemon with the
/// selection moved underneath it, rather than pinning `detach_agent_on` in
/// isolation and proving nothing about who calls it.
///
/// # The deck comes from the request (PRD #1223 U4)
///
/// `deck_id` is resolved with [`crate::dto::DeckScope::resolve`], as the start
/// actions resolve theirs, and never from the selection — so an agent started
/// on another deck from the overview under **All Decks** is stopped on THAT
/// deck. An id the app no longer observes is refused with the resolve error,
/// before anything is asked of any deck.
///
/// The stop is bounded by [`WORKFLOW_ROLE_STOP_TIMEOUT`], so a deck that takes
/// the connection and never answers ends the action with a sentence rather
/// than holding the confirmation open.
async fn stop_agent_action(
    state: &DesktopState,
    deck_id: &str,
    agent_id: &str,
) -> Result<crate::dto::DeckScope, String> {
    validate_agent_id(agent_id)?;
    let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    match tokio::time::timeout(
        WORKFLOW_ROLE_STOP_TIMEOUT,
        daemon.client.stop_agent(agent_id),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(safe_message(error.to_string())),
        Err(_) => {
            return Err(format!(
                "the deck did not answer the stop within {}s",
                WORKFLOW_ROLE_STOP_TIMEOUT.as_secs()
            ));
        }
    }
    // Preserve a working attachment when stop fails: this line is after the `?`
    // above, so a refused stop leaves the terminal alone. Once the daemon
    // confirms, remove the registry entry promptly; the stream reader will also
    // observe STREAM_END and is generation-guarded against removing a newer
    // attachment.
    //
    // The deck is the SCOPE's — the one this operation authenticated against
    // and stopped the agent on — and never a fresh read of the selection.
    terminal::detach_agent_on(state, &scope.identity(), agent_id).await;
    Ok(scope)
}

/// PRD #1223 U4 — close a whole orchestration on the deck `deck_id` names:
/// [`stop_roles_concurrently`] over every role the webview listed, then detach
/// each confirmed role's terminal.
///
/// The deck is resolved once, as [`stop_agent_action`] resolves it, and
/// returned beside the outcome whenever it resolved — including when a stop
/// was not confirmed — so the caller can refresh THAT deck either way: some
/// roles may have stopped.
///
/// A role whose stop was refused or not answered within the bound is named, as
/// data, in the [`LaunchFailure`] — the same shape a launch's rollback reports,
/// so the webview shows it with the same cleanup warning.
async fn stop_orchestration_action(
    state: &DesktopState,
    deck_id: &str,
    roles: &[crate::dto::StopOrchestrationRole],
) -> (Option<crate::dto::DeckScope>, Result<(), LaunchFailure>) {
    if roles.is_empty() {
        return (
            None,
            Err("an orchestration close names no role to stop"
                .to_string()
                .into()),
        );
    }
    for role in roles {
        if let Err(error) = validate_agent_id(&role.agent_id) {
            return (None, Err(error.into()));
        }
    }
    let scope = match crate::dto::DeckScope::resolve(Some(deck_id)) {
        Ok(scope) => scope,
        Err(error) => return (None, Err(error.into())),
    };
    let daemon = match state.daemon.trusted(scope.endpoint()).await {
        Ok(daemon) => daemon,
        Err(error) => return (Some(scope), Err(error.into())),
    };
    if let Err(error) = daemon.require_compatible() {
        return (Some(scope), Err(error.into()));
    }
    let started: Vec<StartedRole> = roles
        .iter()
        .map(|role| StartedRole {
            agent_id: role.agent_id.clone(),
            role: role.name.clone(),
        })
        .collect();
    let outcomes = stop_roles_concurrently(&*daemon.client, &started).await;
    let mut unconfirmed = Vec::new();
    for (role, outcome) in started.iter().zip(outcomes) {
        match outcome {
            // A refused or unanswered stop leaves its terminal alone, as
            // `stop_agent_action` does: the agent may still be running.
            Some(stop) => unconfirmed.push(stop),
            None => terminal::detach_agent_on(state, &scope.identity(), &role.agent_id).await,
        }
    }
    if unconfirmed.is_empty() {
        return (Some(scope), Ok(()));
    }
    let message = format!(
        "could not confirm stop for {} of {} role(s): {}",
        unconfirmed.len(),
        started.len(),
        unconfirmed
            .iter()
            .map(|stop| format!("{} ({})", safe_message(&stop.role), stop.reason))
            .collect::<Vec<_>>()
            .join(", ")
    );
    (
        Some(scope),
        Err(LaunchFailure {
            message,
            unconfirmed_stops: unconfirmed.into_iter().map(|stop| stop.role).collect(),
        }),
    )
}

/// What the webview asked a [`DesktopAction::StartAgent`] to spawn, minus the
/// deck. Grouped so [`start_agent_action`] takes the deck and the request as
/// two arguments rather than seven.
struct StartAgentRequest {
    command: Option<String>,
    cwd: Option<String>,
    display_name: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    /// PRD #1223 M7: `Some` starts an authoring agent — see
    /// [`start_agent_action`]'s authoring section.
    authoring_kind: Option<AuthoringKind>,
}

/// What an authoring start aimed at a deck without the `authoring-kind`
/// capability answers (PRD #1223 M7). An `Err`, so the dialog shows it inline
/// and stays open: the deck is fine, it simply cannot compose the seed.
fn authoring_unsupported_message(kind: AuthoringKind) -> String {
    format!(
        "This deck cannot start a `{}` agent: it predates daemon-composed authoring seeds, and \
         would start a plain agent with no seed. Nothing was started. Start it from the TUI on \
         that deck's host, or upgrade the deck.",
        kind.as_str()
    )
}

/// A start the target deck accepted.
struct StartedAgent {
    /// The id the target daemon minted. Unique only within that daemon, so it
    /// means nothing without [`Self::scope`]'s deck beside it.
    agent_id: String,
    /// The deck the agent was started on — captured once, before the first
    /// await, and the only deck any later step of the action may name.
    scope: crate::dto::DeckScope,
}

/// Start one plain agent on the deck `deck_id` names (PRD #1223 M3).
///
/// # The deck comes from the request, never from the selection
///
/// This was the `StartAgent` arm reaching its daemon through `trusted_daemon()`,
/// which resolves the applied selection — and `Selection::All` resolves to the
/// local deck (#1083). The overview shows every deck at once, so it is exactly
/// the screen where "the selected deck" is least likely to be the one the user
/// meant. [`crate::dto::DeckScope::resolve`] is PRD #1105's answer for terminal
/// attach and it is the same answer here: the id is matched against the
/// observed set, so an id this app is not observing — a deck that disconnected
/// or was removed mid-flow, or a forged one — is refused with that function's
/// error and nothing is started anywhere. There is no retargeting and no
/// fallback to the selection, because a fallback would turn a stale id into a
/// silent spawn on whichever deck is in force.
///
/// # Split out for the reason [`stop_agent_action`] is
///
/// Everything here is testable and the emit around it is not, so a test can
/// drive it against two real daemons with the selection on All Decks.
///
/// # An authoring agent (PRD #1223 M7)
///
/// With `authoring_kind` set, the start goes through
/// [`DaemonClient::start_authoring_agent`], which withholds the field from a
/// deck that does not advertise it — an older deck would drop it and start a
/// plain agent with no seed — so such a deck answers
/// [`authoring_unsupported_message`] and nothing is sent. Every other part of
/// the start is the plain one: the same deck capture, the same minted
/// `DOT_AGENT_DECK_PANE_ID` (which the deck requires here, because the seed is
/// delivered to that pane), and the same last-command record.
///
/// The command must already be resolved. A blank one means the deck's default
/// shell, which cannot act on a seed, so the dialog resolves it the way the
/// TUI's `resolve_authoring_command` does and this refuses one that arrives
/// blank rather than resolving it a second, divergent way. A `cwd` is required
/// for the deck's reason: the seed names the directory the agent works in.
///
/// # A named directory must be absolute (PRD #1223 audit D2)
///
/// [`validate_start_fields`] checks a `cwd`'s bytes and length, not its shape,
/// so a relative `repo` would start an agent relative to wherever that deck's
/// daemon was spawned from. The dialog now only sends a path a deck listed
/// (PRD #1223 U1 removed the typed path), but this is the action boundary, so
/// a present `cwd` still gets the string-shape check [`list_directories_on`]
/// gives a path,
/// [`validate_pasted_project_path`], before any deck is asked. An absent one
/// stays allowed: that is the deck's default-directory start.
async fn start_agent_action(
    state: &DesktopState,
    deck_id: &str,
    request: StartAgentRequest,
) -> Result<StartedAgent, String> {
    let StartAgentRequest {
        command,
        cwd,
        display_name,
        rows,
        cols,
        authoring_kind,
    } = request;
    let (rows, cols) = validate_start_fields(
        command.as_deref(),
        cwd.as_deref(),
        display_name.as_deref(),
        rows.unwrap_or(24),
        cols.unwrap_or(80),
    )?;
    if let Some(cwd) = cwd.as_deref() {
        validate_pasted_project_path(cwd)?;
    }
    if let Some(kind) = authoring_kind {
        if command.is_none() {
            return Err(format!(
                "a `{}` agent needs a command that starts an agent; an empty one would start the deck's default shell",
                kind.as_str()
            ));
        }
        if cwd.is_none() {
            return Err(format!(
                "a `{}` agent needs a directory for its seed to name",
                kind.as_str()
            ));
        }
    }
    // ONE capture, before the first await (issue #1116).
    let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
    let agent_type = AgentType::from_command(command.as_deref());
    let pane_id = mint_desktop_pane_id();
    // Kept for the per-deck last command (PRD #1223 M4), recorded only once the
    // deck has accepted the start — a refused start leaves the value it had.
    // An authoring start records too, as the TUI's `record_candidate` does for
    // every form-submitted command.
    let requested_command = command.clone();
    let options = StartAgentOptions {
        command,
        cwd,
        display_name,
        rows,
        cols,
        env: vec![(DOT_AGENT_DECK_PANE_ID.into(), pane_id)],
        agent_type,
        ..Default::default()
    };
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    // PRD #1223 audit F4: bounded like every role start, so the New agent
    // dialog — which cannot be closed while a start is in flight (audit F5) —
    // always gets an answer.
    let agent_id = match authoring_kind {
        None => bounded_plain_start(daemon.client.start_agent(options)).await?,
        Some(kind) => {
            match bounded_plain_start(daemon.client.start_authoring_agent(options, kind)).await? {
                GatedQuery::Answered(agent_id) => agent_id,
                GatedQuery::Unsupported => return Err(authoring_unsupported_message(kind)),
            }
        }
    };
    if let Some(command) = requested_command.as_deref() {
        state.remember_last_command(&scope.identity(), command);
    }
    Ok(StartedAgent { agent_id, scope })
}

/// One plain or authoring start under [`WORKFLOW_ROLE_START_TIMEOUT`] (PRD #1223
/// audit F4). The elapsed case says what it is: the deck may still start the
/// agent, so the user is told to look before starting a second one.
async fn bounded_plain_start<T, E: std::fmt::Display>(
    start: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(WORKFLOW_ROLE_START_TIMEOUT, start).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(safe_message(error.to_string())),
        Err(_) => Err(format!(
            "the deck did not answer the start within {}s; the agent may still appear, so check \
             the deck before starting it again",
            WORKFLOW_ROLE_START_TIMEOUT.as_secs()
        )),
    }
}

/// The fields of [`DesktopAction::StartOrchestration`] after its deck id.
struct StartOrchestrationRequest {
    path: String,
    orchestration: String,
    display_title: Option<String>,
    config_revision: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
}

/// An orchestration launch the target deck accepted.
struct StartedOrchestration {
    /// The start role's agent — the pane the dialog opens.
    start_agent_id: String,
    /// Every role's agent, in the order they were started.
    agent_ids: Vec<String>,
    /// The deck, captured once before the first await.
    scope: crate::dto::DeckScope,
}

/// [`DesktopAction::StartWorkflow`]'s fields, unpacked for
/// [`start_workflow_action`].
struct StartWorkflowRequest {
    name: String,
    cwd: String,
    task_prompt: String,
    roles: Vec<WorkflowRoleInput>,
    rows: Option<u16>,
    cols: Option<u16>,
    config_revision: Option<String>,
}

/// The Runs screen's workflow launch ([`DesktopAction::StartWorkflow`]).
///
/// # It launches on the SELECTED deck, and never under All Decks
///
/// The Runs screen shows one deck, so its launch names none and goes to the
/// selected one through [`trusted_daemon`]. Under **All Decks** that refuses
/// before any deck is contacted (#1083): the selection resolves to the local
/// deck there only because the plumbing needs an endpoint, and a launch that
/// took it would start agents on this machine because the user chose every
/// deck. The webview shows "Select a deck" instead of the launch form in that
/// state; this is the backstop. Split out of the action arm so that property
/// can be driven against a real daemon
/// (`daemon_bridge::tests::a_runs_launch_under_all_decks_never_reaches_the_local_deck`).
async fn start_workflow_action(
    state: &DesktopState,
    request: StartWorkflowRequest,
) -> Result<WorkflowLaunchResult, DesktopActionError> {
    let StartWorkflowRequest {
        name,
        cwd,
        task_prompt,
        roles,
        rows,
        cols,
        config_revision,
    } = request;
    ensure_desktop_workflow_platform_supported(std::env::consts::OS)?;
    let (rows, cols) =
        validate_workflow_shape(&name, &cwd, &roles, rows.unwrap_or(32), cols.unwrap_or(120))?;
    // PRD #819 M6: the connection comes FIRST now. Resolution used to
    // run two lines above the first daemon contact, against this
    // process's own filesystem; it now runs on the daemon's, so a
    // connection has to exist before a launch can be prepared at all.
    // The supported non-Pi coordinator still uses the readiness-gated,
    // identity-bound retry path in `launch_workflow`; Pi is rejected
    // inside the preparation, before anything is spawned.
    let daemon = trusted_daemon(&state.daemon).await?;
    daemon.require_compatible()?;
    ensure_daemon_can_prepare(daemon.client.cached_capabilities().as_ref())?;
    let (roles, prepared) = prepare_workflow_launch(
        daemon.client.as_ref(),
        &name,
        &cwd,
        &task_prompt,
        &roles,
        config_revision.as_deref(),
    )
    .await?;
    let orchestration_id = mint_orchestration_id();
    launch_workflow(
        daemon.client.as_ref(),
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
    .await
    .map_err(|failure| DesktopActionError::launch(failure.message, failure.unconfirmed_stops))
}

/// Launch one of a project's orchestrations on the deck `deck_id` names, the
/// TUI's way (PRD #1223 M6): prepare with **no task**, then start every role
/// with the command its config gives it, on that deck.
///
/// # The deck comes from the request
///
/// For [`start_agent_action`]'s reason and through the same
/// [`crate::dto::DeckScope::resolve`] capture: the preparation, every role start
/// and the coordinator delivery all go to the one deck the dialog chose, and an
/// id this app no longer observes is refused before anything is asked.
///
/// # What it deliberately does not inherit from the Runs launch
///
/// The Runs screen's [`DesktopAction::StartWorkflow`] refuses an empty task,
/// builds each role's command from desktop agent profiles, refuses a Pi
/// coordinator (its desktop-side delivery needs an acknowledgement Pi's native
/// seed cannot give) and refuses Windows (its profile commands are POSIX-quoted).
/// None of those reasons holds here: there is no task, the deck runs its own
/// configured commands, and a Pi coordinator is seeded by the deck exactly as
/// the TUI's is — see [`launch_configured_orchestration`]. The Runs screen keeps
/// all four.
///
/// # Before preparing
///
/// A deck that cannot start a role with its configured command is refused
/// before `prepare-workflow` — see [`orchestration_launch_unavailable`] — so it
/// is not asked to publish a coordinator context nothing will read.
async fn start_orchestration_action(
    state: &DesktopState,
    deck_id: &str,
    request: StartOrchestrationRequest,
) -> Result<StartedOrchestration, DesktopActionError> {
    let StartOrchestrationRequest {
        path,
        orchestration,
        display_title,
        config_revision,
        rows,
        cols,
    } = request;
    validate_pasted_project_path(&path)?;
    if !is_valid_display_name(&orchestration) {
        return Err(
            "orchestration name is invalid, oversized, empty, or contains control characters"
                .into(),
        );
    }
    if let Some(title) = display_title.as_deref()
        && !is_valid_display_name(title)
    {
        return Err("the run name is invalid, oversized, or contains control characters".into());
    }
    let (rows, cols) = validate_dimensions(rows.unwrap_or(32), cols.unwrap_or(120))?;
    // ONE capture, before the first await (issue #1116).
    let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    if let Some(reason) = orchestration_launch_unavailable(&daemon).await? {
        return Err(reason.into());
    }
    ensure_one_orchestration_of_that_name(&daemon, &path, &orchestration).await?;
    // Deliberately NOT under a client-side deadline, unlike every other call
    // this launch makes (PRD #1223 audit V1). The deck resolves, composes,
    // issues the token and publishes `orchestrator-context.md` on its blocking
    // pool, and dropping this future cannot stop that: a preparation reported
    // here as timed out would still publish afterwards, possibly over a
    // retry's context once the retry's last prepared-role check has passed.
    // The role starts and rollback stops below stay bounded, and for two
    // different reasons (audit W6 — this comment used to give the start's
    // reason for both). A start that elapses is reported as INDETERMINATE and
    // goes through the pane-and-orchestration reconciliation, so one that
    // landed anyway is found and stopped. A stop that elapses is reconciled
    // against nothing at all: `rollback_workflow_agents` records it as an
    // unconfirmed stop and the launch's error names the role, which is a
    // report to the user rather than a remedy — but it does bound what the
    // rollback costs and lets it reach the roles behind a wedged stop. A
    // preparation has neither: dropping it neither finds out what happened nor
    // says anything useful, so it is waited out instead.
    let prepared = daemon
        .client
        .prepare_workflow(&path, &orchestration, "", config_revision.as_deref())
        .await
        .map_err(|error| safe_message(error.to_string()))?;
    // Both are `#[serde(default)]` on the reply, and neither may be invented
    // here — see `prepare_workflow_launch`.
    if prepared.path.is_empty() {
        return Err(
            "the deck prepared the orchestration but reported no canonical project path; nothing was started"
                .into(),
        );
    }
    if prepared.prompt.trim().is_empty() {
        return Err(
            "the deck prepared the orchestration but reported no coordinator prompt; nothing was started"
                .into(),
        );
    }
    let launched = launch_configured_orchestration(
        daemon.client.as_ref(),
        &orchestration,
        display_title.as_deref(),
        &prepared,
        rows,
        cols,
        &mint_orchestration_id(),
    )
    .await
    .map_err(|failure| DesktopActionError::launch(failure.message, failure.unconfirmed_stops))?;
    Ok(StartedOrchestration {
        start_agent_id: launched.start_agent_id,
        agent_ids: launched.agent_ids,
        scope,
    })
}

/// The dialog's reason for a namesake orchestration, in the crate (PRD #1223
/// audit V4) — the same sentence `ambiguousOrchestrationReason` builds in
/// `desktop/src/lib/newAgent.ts`, plus what an action has to say that a
/// disabled chip does not.
fn ambiguous_orchestration_refusal(orchestration: &str) -> String {
    format!(
        "This project defines more than one orchestration named {}; rename one to launch it here. \
         Nothing was started.",
        safe_message(orchestration)
    )
}

/// PRD #1223 audit V4: refuse a launch whose orchestration name names MORE
/// than one of the project's orchestrations on that deck.
///
/// `PrepareWorkflow` takes the FIRST role-bearing definition with the name
/// (`project_resolve.rs`, the same rule the TUI's spawn uses), so launching a
/// namesake would run the other definition's roles and commands under the name
/// the user chose. Config validation only warns about the duplicate, and the
/// name is all the wire carries.
///
/// The dialog already shows namesakes as disabled chips and never submits one
/// (audit F2), but that is presentation: this is the boundary every caller
/// crosses — the main webview's own action, a frontend regression, a fixture
/// caller — so the invariant is checked where the launch is decided.
///
/// **Desktop-side only, deliberately.** Changing `PrepareWorkflow`'s
/// first-match rule would change an existing verb that older desktops and the
/// TUI already call, which is not this PR's to do; see issue #1233.
///
/// A name the project defines NO orchestration under is deliberately left to
/// the deck: its `PrepareWorkflow` refuses that before it composes or publishes
/// anything, in its own words and with its own stable code, so refusing it here
/// would only be a second copy of that sentence in a crate that is not allowed
/// to resolve projects itself (`xtask/linkage-check` rule 12). The roleless
/// entries the daemon's lookup skips are not in this listing either — the
/// resolve projection drops them — so the two count the same definitions.
async fn ensure_one_orchestration_of_that_name(
    daemon: &crate::daemon_bridge::TrustedDaemon,
    path: &str,
    orchestration: &str,
) -> Result<(), DesktopActionError> {
    // Bounded (PRD #1223 audit W3), unlike the preparation below. The reason
    // that one is not — dropping the future cannot stop the publish it has
    // already started — does not apply to a read: `ResolveProject` writes
    // nothing and is idempotent, so a deck that takes the connection and never
    // answers costs this and the launch fails rather than holding the dialog's
    // **Starting…** open for as long as the peer holds the socket. Its sibling
    // check, `orchestration_launch_unavailable`, already bounds its handshake.
    let project =
        crate::daemon_bridge::bounded_reply("ResolveProject", daemon.client.resolve_project(path))
            .await?;
    let defined = project
        .orchestrations
        .iter()
        .filter(|candidate| candidate.name == orchestration)
        .count();
    if defined > 1 {
        return Err(ambiguous_orchestration_refusal(orchestration).into());
    }
    Ok(())
}

/// The target deck's snapshot after a start, for the direct refresh that
/// follows it (PRD #1223 M3) — `None` when the fleet moved while it was taken.
///
/// # Why the action refreshes the target and not only the selected deck
///
/// Every `DesktopAction` tails `refresh_and_emit`, which snapshots the
/// **selected** deck. A start on another deck would then appear only when that
/// deck's watcher next re-fetched, and the daemon's `StartAgent` handler
/// broadcasts nothing, so for an agent with no hooks that is the five-second
/// reconcile.
///
/// # Checked on both sides of the await
///
/// Before, so a deck that left the fleet since the start is not re-handshaken
/// for a snapshot nobody will show. After, because this is a publication: a
/// snapshot emitted for a deck that left while it was being taken would put
/// that deck's group back on an overview that has just pruned it. The start
/// itself is not undone either way — the agent is running, and saying
/// otherwise would be worse than the watcher showing it late.
async fn target_deck_snapshot(
    links: &DaemonLinks,
    scope: &crate::dto::DeckScope,
) -> Option<DesktopSnapshot> {
    scope.revalidate().ok()?;
    let snapshot = snapshot_with(scope.endpoint(), links, None).await;
    scope.revalidate().ok()?;
    Some(snapshot)
}

/// [`apply_selection`] minus the emit, reporting whether the deck moved.
///
/// Split out because everything above the emit is testable and the emit is not —
/// it needs an `AppHandle`, which means a running Tauri app. The one thing worth
/// pinning here is exactly the thing a running app makes hard to observe: that an
/// ordinary settings save does **not** take the switch path.
async fn retarget_selection(state: &DesktopState, settings: &DesktopSettings) -> bool {
    let previous = crate::dto::selected_endpoint().identity();
    let deck = crate::dto::apply_settings_selection(settings);
    let key = deck.endpoint.identity();
    let moved = selection_moved(&previous, &key);
    let observed = observed_keys(settings);
    // PRD #1105 — the sessions on decks that LEFT the observed set, and no
    // others. This was `detach_all` gated on `moved`; see
    // `terminal::detach_decks_outside` for why a selection move is no longer a
    // reason to tear a terminal down, and why this one is not gated.
    //
    // Before the tunnels are released, so a DETACH frame still has a transport.
    terminal::detach_decks_outside(state, &observed).await;
    state.daemon.invalidate_all().await;
    state.tunnels.retain(&observed).await;
    // PRD #742 M3: the watcher half of the same teardown, and the natural
    // sibling of the `retain` above it — a deck that left the observed set must
    // stop being watched as well as stop holding a transport, or it goes on
    // emitting records into a view that no longer has a group for them.
    //
    // The STARTING half is not here, and cannot be: spawning a watcher needs an
    // `AppHandle`, which is exactly what this function is split out to be
    // without. `apply_selection` does it one line up the stack.
    state.retain_watchers(&observed);
    if moved {
        state.selection_changed();
    }
    moved
}

/// Every deck key the app must keep a transport for under `settings` — PRD
/// #742 M2's live set.
///
/// # Why this is not gated on the deck having moved, and never was
///
/// The switch half of [`apply_selection`] is gated because it is *destructive
/// to something a user can see* — it detaches every terminal — so it has to be
/// told apart from a colour-scheme save. `retain` needs no such gate, and the
/// reason survives the widening from one deck to N: the set is **derived from
/// the document**, so a save that changed no deck produces a byte-identical set
/// and `retain` removes nothing. It was already running unconditionally for
/// that reason and it still does.
///
/// # What "the deck actually moved" becomes for a set
///
/// It does not become anything, and that is the design decision rather than an
/// omission. The three things [`selection_moved`] gates — the detach, the
/// watcher's re-subscribe, and `apply_selection`'s emit — are all about the
/// **one** deck the deck screen and its terminals talk to, which PRD #742
/// DECISION 1 keeps single-deck. So they keep comparing `resolve()`'s resolved
/// key, unchanged and for the reasons [`selection_moved`] already gives.
///
/// The set-level events are real but land on a different consumer. A set that
/// **gained** a member needs a watcher started (PRD #742 M3); a set whose
/// existing member **changed address** needs the old address's transport torn
/// down *and* that member's watcher restarted; a set that **lost** a member
/// needs only the teardown. `retain` over this set already answers the last
/// two halves of that — an edited address is a different [`dot_agent_deck::daemon_client::EndpointIdentity`],
/// so the old one is simply no longer named — and it answers them without
/// knowing which event it was, because a set difference is all a teardown
/// needs. **Starting** a watcher is the half a set difference cannot be read
/// backwards from, and it is M3's to build; deriving it here, with nothing to
/// consume it, would be inventing the signal before its consumer.
///
/// # The resolved deck is always in here
///
/// [`crate::settings::DesktopSettings::connectable_endpoints`] guarantees it by
/// construction, and this is where it is load-bearing: were it not, an ordinary
/// theme save would release the transport under the deck screen's own terminals.
///
/// # A deck with no address is deliberately NOT in here
///
/// PRD #742 M12 split the display set off from this one. A configured row with
/// no socket path is a member of the fleet the overview renders and is not a
/// member of this set, because there is no address to keep a transport for —
/// a watcher for it would spin against an endpoint that cannot exist, and
/// `retain` would be asked to tear down something that was never built. The
/// display side is [`crate::dto::observed_fleet`].
fn observed_keys(
    settings: &DesktopSettings,
) -> std::collections::HashSet<dot_agent_deck::daemon_client::EndpointIdentity> {
    settings
        .connectable_endpoints()
        .iter()
        .map(dot_agent_deck::daemon_client::Endpoint::identity)
        .collect()
}

/// Whether a save changed which deck the app is talking to.
///
/// Compared by [`dot_agent_deck::daemon_client::EndpointIdentity`] — the key
/// both `DaemonLinks` and `EndpointTunnels` are indexed by — rather than by the
/// stored `Selection` token, and the difference is load-bearing in both
/// directions. **Editing the selected deck's address moves the deck without
/// moving the token**, and that has to count: the tunnel, the link and every
/// terminal on it belong to the old address. And a *resolved* key is what the
/// app is actually talking to, so a selection that falls back to the local deck
/// — a row that is gone, a row with no socket path yet — compares as local,
/// which is what it is.
///
/// The converse is the case this exists for: editing a deck the user is **not**
/// on, or changing a theme, leaves the key identical and takes no switch path.
///
/// It compared `Endpoint::describe()` until PRD #741's Greptile P1 review: that
/// string omits the remote socket path, the identity file and the jump host, so
/// editing any of the three on the selected deck moved the deck without moving
/// the comparison — no detach, no tunnel teardown, and the held link stayed
/// pointed at the old route. The identity type is what closes it here and in
/// the two maps at once.
fn selection_moved(
    previous: &dot_agent_deck::daemon_client::EndpointIdentity,
    next: &dot_agent_deck::daemon_client::EndpointIdentity,
) -> bool {
    previous != next
}

/// Test one endpoint end to end and report a **named state** (PRD #741 M10).
///
/// A standalone command rather than a `DesktopAction`, for the same reason the
/// settings commands are: every `DesktopAction` ends in `refresh_and_emit`, so
/// routing this through one would make testing a deck the app is *not* talking
/// to cost a `ListAgents` round trip against the deck it is, and return the
/// answer inside a snapshot that has nowhere to put it.
///
/// It takes the document from the webview rather than re-reading the file,
/// because the row a user is testing is usually one they have just typed and
/// the panel saves optimistically — reading the disk would test the previous
/// value. The document is the same validated `DesktopSettings` the save path
/// takes, so nothing unvalidated reaches ssh.
///
/// **It writes nothing.** A discovered socket path comes back in the report and
/// the panel puts it in the row; see `endpoint_test`'s module docs for why the
/// write-back is not made here.
#[tauri::command]
async fn desktop_test_endpoint(
    webview: Webview,
    state: State<'_, DesktopState>,
    settings: DesktopSettings,
    selection: String,
) -> Result<crate::endpoint_test::EndpointTestReport, String> {
    ensure_main_webview(&webview)?;
    if selection.len() > crate::settings::MAX_ENDPOINT_ID_BYTES {
        return Err(format!(
            "an endpoint id is at most {} bytes",
            crate::settings::MAX_ENDPOINT_ID_BYTES
        ));
    }
    Ok(crate::endpoint_test::test_endpoint(&settings, &selection, &state.tunnels).await)
}

/// Apply a zoom level to the main webview (PRD #744).
///
/// # Why this is a command rather than `getCurrentWebview().setZoom()`
///
/// The frontend route needs the `core:webview:allow-set-webview-zoom`
/// permission, which `core:default` does **not** include — its webview default
/// set is `allow-get-all-webviews`, `allow-webview-position`,
/// `allow-webview-size` and `allow-internal-toggle-devtools`. Broadening the
/// webview's own core surface to reach a call we can make ourselves would be
/// strictly more privilege for no gain. This also keeps the zoom behind the
/// `DeckBridge` seam like every other bridge call, so fixture mode no-ops
/// structurally rather than by a check.
///
/// # It does not persist anything
///
/// Applying and storing are separate on purpose: the webview is told here, and
/// the level is written by [`desktop_set_settings`] behind a coalescer, because
/// a held key would otherwise rewrite `desktop.toml` once per repeat. The
/// launch-time apply in [`run`] reads the stored level and calls the same
/// `set_zoom`.
///
/// The level is snapped to the ladder before it is applied, so a webview
/// sending an arbitrary float cannot drive the window to 40x — the same
/// `ZoomLevel::snap` the document uses, so the two paths cannot disagree about
/// what a level means.
#[tauri::command]
async fn desktop_set_zoom(webview: Webview, level: f64) -> Result<f64, String> {
    ensure_main_webview(&webview)?;
    let snapped = settings::ZoomLevel::snap(level);
    apply_zoom(&webview, snapped);
    Ok(snapped.as_f64())
}

/// Push a level at a webview, logging rather than propagating a failure.
///
/// Shared by the command above and the launch-time apply, so there is one call
/// site for `set_zoom` and not two that could drift. A failure is not worth
/// failing either caller over: the command's caller has already applied the
/// level optimistically in the UI, and a launch that refuses to start because a
/// zoom could not be set would be a far worse bug than a window at 100%.
fn apply_zoom(webview: &Webview, level: settings::ZoomLevel) {
    if let Err(error) = webview.set_zoom(level.as_f64()) {
        eprintln!("dot-agent-deck-desktop: could not set webview zoom: {error}");
    }
}

#[tauri::command]
async fn desktop_run_action(
    app: AppHandle,
    webview: Webview,
    state: State<'_, DesktopState>,
    action: DesktopAction,
) -> Result<DesktopActionResult, DesktopActionError> {
    ensure_main_webview(&webview)?;
    let mut result_agent_id = None;
    let mut result_agent_ids = Vec::new();
    let mut result_send: Option<SendResult> = None;
    let mut result_terminal = None;
    let mut result_message = None;

    match action {
        DesktopAction::Refresh => {}
        DesktopAction::Bootstrap { start_if_missing } => {
            let snapshot = bootstrap(&BootstrapOptions { start_if_missing }, &state.daemon).await;
            emit_snapshot(&app, &snapshot);
            ensure_snapshot_watchers(&app, &state);
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
            deck_id,
            command,
            cwd,
            display_name,
            rows,
            cols,
            authoring_kind,
        } => {
            let started = start_agent_action(
                &state,
                &deck_id,
                StartAgentRequest {
                    command,
                    cwd,
                    display_name,
                    rows,
                    cols,
                    authoring_kind,
                },
            )
            .await?;
            // PRD #1223 M3: the TARGET deck, directly — the tail below
            // refreshes only the selected one. The watcher nudge is what keeps
            // that deck's next watcher emit from answering out of a fold that
            // has never heard of the new agent and taking it off screen again.
            if let Some(snapshot) = target_deck_snapshot(&state.daemon, &started.scope).await {
                emit_snapshot(&app, &snapshot);
            }
            state.request_refetch(&started.scope.identity());
            result_agent_id = Some(started.agent_id);
        }
        DesktopAction::StartOrchestration {
            deck_id,
            path,
            orchestration,
            display_title,
            config_revision,
            rows,
            cols,
        } => {
            let started = start_orchestration_action(
                &state,
                &deck_id,
                StartOrchestrationRequest {
                    path,
                    orchestration,
                    display_title,
                    config_revision,
                    rows,
                    cols,
                },
            )
            .await?;
            // The target deck directly, as after a plain start (PRD #1223 M3).
            if let Some(snapshot) = target_deck_snapshot(&state.daemon, &started.scope).await {
                emit_snapshot(&app, &snapshot);
            }
            state.request_refetch(&started.scope.identity());
            result_agent_id = Some(started.start_agent_id);
            result_agent_ids = started.agent_ids;
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
            let launched = start_workflow_action(
                &state,
                StartWorkflowRequest {
                    name,
                    cwd,
                    task_prompt,
                    roles,
                    rows,
                    cols,
                    config_revision,
                },
            )
            .await?;
            result_agent_id = Some(launched.start_agent_id);
            result_agent_ids = launched.agent_ids;
            result_message = Some(
                "Workflow started from the configured orchestration. Commands were applied for this launch only; profile/model command write-back is not implemented."
                    .into(),
            );
        }
        DesktopAction::StopAgent { deck_id, agent_id } => {
            let scope = stop_agent_action(&state, &deck_id, &agent_id).await?;
            // PRD #1223 U4: the TARGET deck, directly, for the StartAgent
            // arm's reason — the tail below refreshes only the selected one.
            if let Some(snapshot) = target_deck_snapshot(&state.daemon, &scope).await {
                emit_snapshot(&app, &snapshot);
            }
            state.request_refetch(&scope.identity());
            result_agent_id = Some(agent_id);
        }
        DesktopAction::StopOrchestration { deck_id, roles } => {
            let (scope, outcome) = stop_orchestration_action(&state, &deck_id, &roles).await;
            // Refreshed whatever the outcome: a close that could not confirm
            // every stop has still stopped the rest.
            if let Some(scope) = &scope {
                if let Some(snapshot) = target_deck_snapshot(&state.daemon, scope).await {
                    emit_snapshot(&app, &snapshot);
                }
                state.request_refetch(&scope.identity());
            }
            outcome.map_err(|failure| {
                DesktopActionError::launch(failure.message, failure.unconfirmed_stops)
            })?;
            result_agent_ids = roles.into_iter().map(|role| role.agent_id).collect();
        }
        DesktopAction::StopDaemon { force } => {
            // PRD #741 M2: `run_daemon_stop` takes a `LocalEndpoint`, so this
            // cannot reach a remote deck even by accident — `require_local`
            // is the only way to produce one and it refuses, naming the deck
            // and the reason. (M7 renders this as a disabled button carrying
            // the same explanation rather than a failed action.)
            let endpoint = selected_endpoint();
            let local = endpoint
                .require_local("Stop deck")
                .map_err(|error| safe_message(error.to_string()))?;
            let outcome = run_daemon_stop(local, force)
                .await
                .map_err(|error| safe_message(error.to_string()))?;
            // PRD #741 M4(a): the daemon this link was established against is
            // being terminated, so the handshake held for it describes a
            // process that is going away. Drop it here rather than waiting for
            // the watcher to notice its stream end.
            state.daemon.invalidate(&endpoint).await;
            // PRD #1105: this deck's sessions only. Stop is refused for
            // anything but the local deck, so tearing down every deck's
            // terminals would close panes on machines this action never
            // touched.
            terminal::detach_deck(&state, &endpoint).await;
            result_message = Some(match outcome {
                StopOutcome::NoDaemonRunning => "No deck was running.".into(),
                StopOutcome::Stopped { pid } => format!("Deck stopped gracefully (pid {pid})."),
                StopOutcome::ForceKilled { pid } => format!("Deck force-killed (pid {pid})."),
            });
        }
        DesktopAction::RestartDaemon => {
            // Replace daemon is Stop plus a lazy-spawn of the desktop's own
            // bundled build. Both halves are local acts, and on a remote deck
            // the pair would be worse than either: terminate the ssh tunnel,
            // then start a LOCAL daemon and report success. Refused by type.
            let endpoint = selected_endpoint();
            let local = endpoint
                .require_local("Replace deck")
                .map_err(|error| safe_message(error.to_string()))?;
            run_daemon_stop(local, false)
                .await
                .map_err(|error| safe_message(error.to_string()))?;
            // Same as Stop: the held handshake describes the daemon just
            // terminated, and the `bootstrap` below is about to start a
            // different one at the same address.
            state.daemon.invalidate(&endpoint).await;
            // This deck's sessions only, for the same reason Stop's are.
            terminal::detach_deck(&state, &endpoint).await;
            let snapshot = bootstrap(
                &BootstrapOptions {
                    start_if_missing: true,
                },
                &state.daemon,
            )
            .await;
            emit_snapshot(&app, &snapshot);
            ensure_snapshot_watchers(&app, &state);
            ensure_explicit_start_connected(true, &snapshot)?;
            return Ok(DesktopActionResult {
                ok: true,
                agent_id: None,
                agent_ids: Vec::new(),
                send_result: None,
                terminal: None,
                message: Some("Deck replaced with the desktop's matching bundled build.".into()),
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
            // PRD #741 M4(a): this action's ENTIRE effect is that the handshake
            // must be classified again — the comment above says so, and since
            // the classification is now held it has to be dropped explicitly.
            // Without this the flag would be set and the banner would keep
            // reporting the refusal it just lifted.
            state.daemon.invalidate_all().await;
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
            let daemon = trusted_daemon(&state.daemon).await?;
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
            // PRD #882: this action carries no measured geometry (it is the
            // declarative attach path, not the tile's own), so it declares no
            // viewport and constrains nothing. The tile's first resize registers
            // its size a frame later.
            // No deck: this action is the legacy declarative attach path and has
            // always meant the selected deck. The webview's own attach command
            // names one.
            let attached =
                terminal::attach(&app, &state, None, agent_id.clone(), channel, None).await?;
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
                )
                .into());
            }
            let daemon = trusted_daemon(&state.daemon).await?;
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

    let snapshot = refresh_and_emit(&app, &state.daemon).await;
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

/// PRD #1105 M11 step 4: the focus state a window event reports, if it reports
/// one. Only `Focused` does; every other window event says nothing about focus.
fn window_focus(event: &tauri::WindowEvent) -> Option<bool> {
    match event {
        tauri::WindowEvent::Focused(focused) => Some(*focused),
        _ => None,
    }
}

/// PRD #802's audit blocker: release the microphone — and, since the sleep
/// work, the machine with it.
///
/// # Why this is Rust's job and not the panel's
///
/// The recording's lifetime is owned here — [`voice::CaptureSession`] holds the
/// device stream and the captured audio, and the webview holds neither. What
/// used to end a recording on teardown was a React passive-effect cleanup in
/// `desktop/src/components/VoiceControlPanel.tsx` making an asynchronous IPC
/// call, and a webview that is going away is not guaranteed to run it, let
/// alone to let it finish: a hard reload, a crashed web-content process, a
/// destroyed window. The device then stayed open with nothing left that could
/// close it, and the replacement panel — which initialises its toggle to *off*
/// — said `Voice off` over it.
///
/// It compounds past the length cap. [`voice::CaptureSession::cap_reached`]
/// releases the device on purpose but KEEPS the audio and leaves the session
/// `Recording`, so the utterance stays the user's to send or discard; a webview
/// lost at that moment left up to 960 KB of captured speech in the session with
/// nothing able to reach it, and every later start refused because
/// `accepts_start` takes neither `Recording` nor `Transcribing`.
///
/// # It releases the sleep inhibit too, and NOT because this function
/// remembers to
///
/// Voice holds a second invisible resource: a sleep inhibit, taken so a machine
/// being driven by speech — which generates no input events — is not suspended
/// by its own idle timer. It leaks the same ways the device does and costs more
/// when it leaks, since a stuck one keeps a laptop awake indefinitely with
/// nothing on screen to explain it.
///
/// So every teardown trigger below releases both. What makes that true is not
/// this function and not a convention at the call sites: [`voice::VoiceHold`]
/// keeps the capture session **private**, so `release` is the only cancel this
/// file can spell, and `release` does both. See that module's docs.
///
/// # What it costs where it is called
///
/// `release` is idempotent and never refused, so every call site can be
/// unconditional. It drops the stream, and a real `CpalStream::drop` joins the
/// device thread — so this is as slow as the platform's own teardown, on
/// whichever thread calls it. That is accepted deliberately at all three call
/// sites: they are the window going away, the document being replaced and the
/// app exiting, and a release that is *scheduled* rather than done is a release
/// that may not happen before the process does. The inhibit goes first and is
/// the cheap half — it cannot be what makes this slow.
fn release_microphone(voice: &VoiceState) -> voice::CaptureStatus {
    voice.hold.release()
}

/// Whether a window event means the webview holding the Voice button is gone.
///
/// `CloseRequested` as well as `Destroyed`, because the two are not one
/// sequence: a close the app vetoes never reaches `Destroyed`, and a close it
/// does not veto reaches it after the window is already unusable. Releasing on
/// both is harmless — [`release_microphone`] is idempotent — and releasing on
/// only the second would mean the microphone outlived the window whenever the
/// platform delivered no `Destroyed` at all.
///
/// Every other window event — focus, move, resize, theme — says nothing about
/// whether the webview is still there, and must not release a live recording.
fn window_ends_capture(event: &tauri::WindowEvent) -> bool {
    matches!(
        event,
        tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
    )
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .manage(DesktopState::default())
        // PRD #802 M7: the capture session. Opens no device until a `start`.
        .manage(VoiceState::default())
        // Issue #845: the stored Light/Dark choice reaches the document root
        // before the webview parses the document, so the first painted frame is
        // already the one the user chose. Registered before `build()`, which is
        // what puts it in the plugin store ahead of the config-declared window.
        .plugin(appearance::init())
        // PRD #744: apply the stored zoom before the webview has run any of our
        // JavaScript, so a user at 150% does not watch the app paint at 100%
        // and then jump. That is the whole reason this leg exists Rust-side;
        // the frontend applies only on *change*.
        //
        // Note this is a deliberate departure from PRD #743's arrangement,
        // whose one effect applies the appearance on load AND on change
        // precisely so there is "no second path that could disagree with this
        // one" (`App.tsx`). Appearance now has a Rust-side pre-paint leg too
        // (issue #845, the `.plugin(..)` above), so that is no longer what
        // separates them — the split is on the *frontend* side: appearance's
        // effect still applies on load as well as on change, so the two legs
        // agree by construction, while zoom's frontend applies only on change
        // and this leg is the sole thing that sets the initial level. Zoom
        // therefore pays the price #743 warns about and appearance does not:
        // if this leg regresses, the window comes up at 100% while the
        // Settings row reads the stored level. `prds/744-*.md` records the
        // condition under which the split should be given up.
        //
        // A missing window is not an error. `load_snapshot` never fails, and a
        // default level makes this a no-op rather than a special case.
        .setup(|app| {
            init_features();
            let stored = settings::load_snapshot().settings;
            // PRD #741 M7: the stored deck selection goes into force before the
            // first snapshot, so the app connects to the deck the user chose
            // rather than to the local one and then switching. An unresolvable
            // selection falls back to local **with a reason**, which the
            // connection banner renders — a silent substitution is how a user
            // ends up acting on the wrong machine's agents.
            crate::dto::apply_settings_selection(&stored);
            if let Some(window) = app.get_webview_window("main") {
                apply_zoom(window.as_ref(), stored.zoom.level);
                // PRD #1105 M11: seed the focus state, in case the window came up
                // focused before a `Focused` event could be delivered. Nothing is
                // attached yet, so there is nothing to claim — this only decides
                // whether the first pane opened claims on its deck.
                if let Ok(focused) = window.is_focused() {
                    app.state::<DesktopState>().set_window_focused(focused);
                }
            }
            Ok(())
        })
        // PRD #1105 M11 step 4: the window's focus changes are the desktop's
        // focus signal — see `terminal::window_focus_changed` for why this event
        // rather than the webview's own, which decks it claims on, and why typing
        // does not also claim. Handled right here rather than in a task spawned
        // per event, so reports are recorded in the order the event loop
        // delivers them; the claims themselves are spawned inside it, and are
        // dropped by the next report if still unsent.
        .on_window_event(|window, event| {
            if let Some(focused) = window_focus(event) {
                let _ = terminal::window_focus_changed(&window.state::<DesktopState>(), focused);
            }
            // PRD #802's audit blocker, teardown trigger 1: the window holding
            // the Voice button is going away, so the microphone goes with it.
            // See `release_microphone` for why the webview's own cleanup is not
            // enough, and `window_ends_capture` for which events count.
            //
            // `try_state` rather than `state`: the latter panics when nothing is
            // managed, and this runs on the event loop where a panic takes the
            // app with it. The voice state IS managed — first line of this
            // builder — so the `None` arm is unreachable rather than a case.
            if window.label() == "main"
                && window_ends_capture(event)
                && let Some(voice) = window.try_state::<VoiceState>()
            {
                release_microphone(&voice);
            }
        })
        // PRD #802's audit blocker, teardown trigger 2: the DOCUMENT is being
        // replaced without the window going anywhere — a hard reload, or a
        // webview the platform lost and recreated. The panel that was holding
        // the microphone is gone by the time this fires, and the one about to
        // mount initialises its toggle to off, so this is the release its own
        // cleanup could not be relied on to make.
        //
        // `Started` rather than `Finished`: the old document is already gone and
        // the new one has not run any of our JavaScript, which is exactly the
        // window in which nothing else would close the device. The app's first
        // load reaches here too and is a no-op, since the session is idle.
        .on_page_load(|webview, payload| {
            if webview.label() == "main"
                && payload.event() == tauri::webview::PageLoadEvent::Started
                && let Some(voice) = webview.try_state::<VoiceState>()
            {
                release_microphone(&voice);
            }
        });
    // PRD #802's audit blocker, teardown trigger 3: the web content PROCESS
    // died. The chain is broken here rather than continued because this hook
    // exists only on Apple's platforms — `Builder::on_web_content_process_
    // terminate` is `#[cfg(any(target_os = "macos", target_os = "ios"))]` in
    // tauri itself, and a `#[cfg]` cannot be attached to one call in a method
    // chain.
    //
    // **So the crash case is covered on macOS and NOT on Linux**, which is
    // worth stating rather than leaving to be discovered: Tauri exposes no
    // equivalent for WebKitGTK, so a Linux web-process crash is released by
    // whichever of the other two triggers arrives first — the reload that
    // follows it, or app exit. Nothing here can do better without a hook that
    // does not exist.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let app = app.on_web_content_process_terminate(|webview| {
        if webview.label() == "main"
            && let Some(voice) = webview.try_state::<VoiceState>()
        {
            release_microphone(&voice);
        }
    });
    let app = app
        .invoke_handler(tauri::generate_handler![
            desktop_get_snapshot,
            desktop_list_projects,
            desktop_resolve_project,
            desktop_new_agent_orchestrations,
            desktop_list_directories,
            desktop_new_agent_options,
            desktop_bootstrap,
            desktop_terminal_attach,
            desktop_terminal_write,
            desktop_terminal_resize,
            desktop_terminal_detach,
            desktop_features,
            desktop_get_settings,
            desktop_set_settings,
            desktop_test_endpoint,
            desktop_set_zoom,
            desktop_run_action,
            desktop_secret_status,
            desktop_store_secret,
            desktop_forget_secret,
            desktop_voice_start,
            desktop_voice_stop,
            desktop_voice_status,
            desktop_voice_cancel,
            desktop_voice_resolve,
            desktop_voice_commands,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build dot-agent-deck desktop application");
    app.run(|app_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
        ) {
            let state = app_handle.state::<DesktopState>();
            let voice_state = app_handle.state::<VoiceState>();
            tauri::async_runtime::block_on(release_on_exit(&state, &voice_state));
        }
    });
}

/// Everything the app lets go of on its way out.
///
/// Extracted from the `RunEvent` closure so it can be driven from a test: the
/// closure itself needs a built `tauri::App` and a real event loop, and what is
/// worth pinning is the release rather than the plumbing that calls it.
///
/// **The microphone goes FIRST**, and the order is the point rather than
/// housekeeping (PRD #802's audit blocker, teardown trigger 4). The sleep
/// inhibit goes with it, inside the same call — see [`release_microphone`].
/// This runs on
/// `ExitRequested` as well as `Exit`, and the two steps after it are the slow
/// ones — a detach writes a frame over a transport that may be an `ssh` child
/// on its way out, bounded at 250 ms *per session*, and the tunnels close after
/// that. Releasing the device behind them would leave a microphone open for the
/// whole of a teardown that has nothing to do with it, and an exit the platform
/// does not wait out would leave it open for good.
async fn release_on_exit(state: &DesktopState, voice: &VoiceState) {
    release_microphone(voice);
    terminal::detach_all(state).await;
    // PRD #741 M7, teardown trigger 4: every `ssh -N -L` child this process
    // owns dies with the app. `Drop` on the last lease is what actually signals
    // the process group; this is what drops the map's handle so there is a last
    // lease to drop.
    state.tunnels.close_all().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn voice_listing(entries: usize) -> voice::VoiceDirectories {
        voice::VoiceDirectories {
            deck_id: "deck-0000000000000001".to_string(),
            path: "/home/dev".to_string(),
            has_parent: true,
            entries: (0..entries)
                .map(|index| voice::VoiceDirectoryEntry {
                    name: format!("dir-{index}"),
                    path: format!("/home/dev/dir-{index}"),
                })
                .collect(),
        }
    }

    /// PRD #1223: a directory declaration as large as a deck can list is
    /// accepted, and one no real browser could have produced is refused.
    #[test]
    fn voice_directory_declarations_are_bounded() {
        assert!(validate_voice_directories(&voice_listing(0)).is_ok());
        assert!(validate_voice_directories(&voice_listing(MAX_VOICE_DIRECTORY_ENTRIES)).is_ok());
        assert!(
            validate_voice_directories(&voice_listing(MAX_VOICE_DIRECTORY_ENTRIES + 1)).is_err()
        );

        let mut long_name = voice_listing(1);
        long_name.entries[0].name = "x".repeat(MAX_VOICE_DIRECTORY_NAME_BYTES + 1);
        assert!(validate_voice_directories(&long_name).is_err());

        let mut long_path = voice_listing(1);
        long_path.path = "/".repeat(MAX_VOICE_DIRECTORY_PATH_BYTES + 1);
        assert!(validate_voice_directories(&long_path).is_err());

        let mut long_entry_path = voice_listing(1);
        long_entry_path.entries[0].path = "/".repeat(MAX_VOICE_DIRECTORY_PATH_BYTES + 1);
        assert!(validate_voice_directories(&long_entry_path).is_err());

        let mut long_deck = voice_listing(1);
        long_deck.deck_id = "d".repeat(MAX_VOICE_DECK_ID_BYTES + 1);
        assert!(validate_voice_directories(&long_deck).is_err());
    }

    /// PRD #1223: a New agent form declaration is bounded like a listing —
    /// a real form's closed sets are accepted, a payload is refused.
    #[test]
    fn voice_new_agent_declarations_are_bounded_and_webview_shaped() {
        let parsed: voice::VoiceNewAgent = serde_json::from_value(serde_json::json!({
            "form": {
                "deckId": "deck-1",
                "path": "/home/dev/code",
                "modes": [{ "id": "none", "label": "No mode" }],
                "agentTypes": [{ "id": "claude", "label": "Claude Code" }],
            },
        }))
        .expect("parses");
        assert!(validate_voice_new_agent(&parsed).is_ok());
        let open_without_form: voice::VoiceNewAgent =
            serde_json::from_value(serde_json::json!({})).expect("a dialog with no live form");
        assert!(open_without_form.form.is_none());
        assert!(
            serde_json::from_value::<voice::VoiceNewAgent>(serde_json::json!({ "command": "rm" }))
                .is_err(),
            "nothing the declaration does not name"
        );

        let mut many = parsed.clone();
        let form = many.form.as_mut().expect("a form");
        form.modes = (0..=MAX_VOICE_FORM_CHOICES)
            .map(|index| voice::VoiceChoice {
                id: format!("mode-{index}"),
                label: format!("mode {index}"),
            })
            .collect();
        assert!(validate_voice_new_agent(&many).is_err());

        let mut long = parsed.clone();
        long.form.as_mut().expect("a form").agent_types[0].label =
            "x".repeat(MAX_VOICE_FORM_CHOICE_BYTES + 1);
        assert!(validate_voice_new_agent(&long).is_err());

        let mut long_path = parsed;
        long_path.form.as_mut().expect("a form").path =
            "/".repeat(MAX_VOICE_DIRECTORY_PATH_BYTES + 1);
        assert!(validate_voice_new_agent(&long_path).is_err());
    }

    /// Scenario: the user adds a deck `new-box` in Settings → Decks and says
    /// "switch deck to the new box" before the write reaches disk. The Deck
    /// selector's section arrives with the utterance in the webview's own
    /// shape, and the switch resolves against THAT list — the new row is a
    /// deck voice can name and maps to its selector token — rather than
    /// against `desktop.toml`, which does not have it yet (Qodo on PR #1340).
    /// A section larger than any selector lists, or a row the settings schema
    /// refuses, is refused at the boundary.
    #[test]
    fn selector_voice_decks_come_from_the_section_the_webview_sends() {
        use crate::settings::EndpointSettings;

        let sent: EndpointSettings = serde_json::from_value(serde_json::json!({
            "remote": [{ "id": "newbox01", "host": "new-box", "port": 22, "socket": "/run/deck.sock" }],
            "selection": "local",
        }))
        .expect("the webview's EndpointSettingsDto parses");
        validate_voice_endpoints(&sent).expect("an ordinary selector section");

        let mut decks = voice_decks(&[], None);
        let selections = selector_voice_decks(Some(&sent), &mut decks, None);
        let new_box = decks
            .iter()
            .find(|deck| deck.label == "new-box")
            .expect("the unflushed deck is one voice can name");
        assert_eq!(
            selections
                .get(&new_box.id)
                .map(|selection| selection.token.as_str()),
            Some("newbox01")
        );

        let row = |index: usize| serde_json::json!({ "id": format!("row{index:08}"), "host": "box", "port": 22 });
        let oversized: EndpointSettings = serde_json::from_value(serde_json::json!({
            "remote": (0..=MAX_VOICE_SELECTOR_ROWS).map(row).collect::<Vec<_>>(),
            "selection": "local",
        }))
        .expect("parses; the bound is the validator's");
        assert!(validate_voice_endpoints(&oversized).is_err());

        assert!(
            serde_json::from_value::<EndpointSettings>(serde_json::json!({
                "remote": [{ "id": "evil01", "host": "-oProxyCommand=x", "port": 22 }],
                "selection": "local",
            }))
            .is_err(),
            "a row the settings schema refuses never reaches the resolver"
        );
    }

    /// Scenario: the app shows the local deck (a single-deck selection, so it
    /// observes only that one) and Settings holds a connectable build box,
    /// reached with an SSH key through a jump host, and a new box with no
    /// socket path yet. Voice's decks gain both — keyed the
    /// way the fleet keys them and unable to take a new agent, for the reason
    /// that fits each — and every deck maps to the token the Deck selector
    /// stores for it. "Switch deck to the build box" then dispatches that
    /// row's token, which is what the selector's write takes.
    #[tokio::test]
    async fn selector_voice_decks_add_the_decks_the_selector_lists() {
        use crate::settings::{EndpointId, EndpointSettings, RemoteEndpointSettings, Selection};
        use dot_agent_deck::daemon_client::Endpoint;
        use dot_agent_deck::remote_tunnel::{HostAlias, Hostname, KeyPath, RemoteSocketPath};

        let row = |id: &str, host: &str, socket: bool| {
            let mut row = RemoteEndpointSettings::new(
                EndpointId::parse(id).expect("a valid id"),
                Hostname::parse(host).expect("a valid host"),
            );
            if socket {
                row.socket = Some(RemoteSocketPath::parse("/run/deck.sock").expect("a path"));
            }
            row
        };
        let mut build_box = row("buildbox01", "build-box", true);
        build_box.identity = Some(KeyPath::parse("~/.ssh/id_ed25519").expect("a key path"));
        build_box.jump = Some(HostAlias::parse("bastion").expect("a jump alias"));
        let endpoints = EndpointSettings {
            remote: vec![build_box, row("newbox01", "new-box", false)],
            selection: Selection::Local,
        };
        let build_key = crate::dto::deck_wire_id(&Endpoint::Remote(
            endpoints.remote[0].endpoint().expect("connectable"),
        ));
        let local_key = crate::dto::deck_wire_id(&Endpoint::local());
        let observed = [crate::dto::ObservedDeckDto {
            deck_id: local_key.clone(),
            label: "/run/deck.sock".to_string(),
            deck_kind: "local",
        }];
        let step: Vec<voice::VoiceDeckChoice> =
            serde_json::from_value(serde_json::json!([{ "deckId": local_key }]))
                .expect("the webview's shape parses");

        let mut decks = voice_decks(&observed, Some(&step));
        let selections = selector_voice_decks(Some(&endpoints), &mut decks, Some(&step));
        let find = |id: &str| decks.iter().find(|deck| deck.id == id).expect("listed");
        assert_eq!(decks.len(), 3, "{decks:?}");
        assert_eq!(
            find(&local_key).unavailable,
            None,
            "the observed deck keeps its step"
        );
        assert_eq!(
            find(&build_key).unavailable.as_deref(),
            Some(voice::DECK_NOT_CONNECTED)
        );
        assert_eq!(find(&build_key).label, "build-box");
        assert_eq!(
            find("unconfigured-newbox01").unavailable.as_deref(),
            Some(crate::dto::UNCONFIGURED_DECK_REASON)
        );
        let token = |key: &str| {
            selections
                .get(key)
                .map(|selection| selection.token.as_str())
        };
        assert_eq!(token(&local_key), Some("local"));
        assert_eq!(token(&build_key), Some("buildbox01"));
        assert_eq!(token("unconfigured-newbox01"), Some("newbox01"));
        assert_eq!(
            selections[&local_key].identity, None,
            "local has no address"
        );
        assert_eq!(
            selections[&build_key].identity,
            Some(voice::VoiceDeckIdentity {
                host: "build-box".to_string(),
                user: None,
                port: 22,
                socket: Some("/run/deck.sock".to_string()),
                identity: Some("~/.ssh/id_ed25519".to_string()),
                jump: Some("bastion".to_string()),
            }),
            "the key and the jump host are part of the address"
        );
        // The address is the row less its id — the set the webview's
        // `REMOTE_ADDRESS_FIELDS` names — so a field added to the row without
        // one here reddens this rather than slipping past the rebind guard.
        let keys = |value: serde_json::Value| {
            let mut keys: Vec<String> = value
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect();
            keys.sort();
            keys
        };
        let every_field = voice::VoiceDeckIdentity {
            host: "h".to_string(),
            user: Some("u".to_string()),
            port: 22,
            socket: Some("/s".to_string()),
            identity: Some("~/k".to_string()),
            jump: Some("j".to_string()),
        };
        let mut row_fields = keys(serde_json::to_value(&endpoints.remote[0]).expect("serializes"));
        row_fields.retain(|field| field != "id");
        assert_eq!(
            keys(serde_json::to_value(&every_field).expect("serializes")),
            row_fields
        );
        assert_eq!(
            selections["unconfigured-newbox01"]
                .identity
                .as_ref()
                .map(|identity| identity.socket.clone()),
            Some(None),
            "a row with no socket still has an address to compare"
        );

        let said = "switch deck to the build box";
        let resolver = voice::StubResolver::new().answering(
            said,
            voice::IntentAnswer::new("switch_deck").with_param("deck", "build box"),
        );
        let mut result = voice::handle_utterance(
            &resolver,
            voice::table(),
            voice::Screen::Deck,
            &[],
            &decks,
            None,
            None,
            voice::Transcript::new(said),
        )
        .await;
        voice::address_deck_switch(&mut result.outcome, |id| selections.get(id).cloned());
        assert!(
            matches!(&result.outcome, voice::VoiceOutcome::Dispatch { invoke, params, .. }
                if invoke == "switchDeck" && params[0].value == "buildbox01"
                    && params[0].deck_identity.as_ref().map(|identity| identity.host.as_str())
                        == Some("build-box")),
            "{:?}",
            result.outcome
        );
    }

    /// Scenario: the webview declares the New agent dialog's deck step with an
    /// utterance — one deck eligible, one disabled with the reason the step
    /// shows — and the fleet has a third deck the step does not list. Voice's
    /// decks keep the declared reason word for word, take the unlisted deck as
    /// not yet reported, and with no declaration treat every deck as eligible.
    /// An oversized or unknown-shaped declaration is refused.
    #[test]
    fn voice_decks_take_eligibility_from_the_declared_deck_step() {
        let observed =
            |deck_id: &str, label: &str, deck_kind: &'static str| crate::dto::ObservedDeckDto {
                deck_id: deck_id.to_string(),
                label: label.to_string(),
                deck_kind,
            };
        let fleet = [
            observed("deck-local", "/run/deck.sock", "local"),
            observed("deck-build", "deploy@build-box", "remote"),
            observed("deck-new", "ci@new-box", "remote"),
        ];
        let step: Vec<voice::VoiceDeckChoice> = serde_json::from_value(serde_json::json!([
            { "deckId": "deck-local" },
            { "deckId": "deck-build", "reason": "No deck is listening on the configured socket." },
            { "deckId": "deck-elsewhere", "reason": "not in this fleet" },
        ]))
        .expect("the webview's shape parses");
        assert!(validate_voice_deck_step(&step).is_ok());

        let decks = voice_decks(&fleet, Some(&step));
        let unavailable = |id: &str| {
            decks
                .iter()
                .find(|deck| deck.id == id)
                .expect("an observed deck")
                .unavailable
                .clone()
        };
        assert_eq!(
            decks.len(),
            3,
            "the fleet, not the declaration, lists the decks"
        );
        assert_eq!(unavailable("deck-local"), None);
        assert_eq!(
            unavailable("deck-build").as_deref(),
            Some("No deck is listening on the configured socket.")
        );
        assert_eq!(
            unavailable("deck-new").as_deref(),
            Some(voice::DECK_NOT_REPORTED)
        );
        assert!(
            voice_decks(&fleet, None)
                .iter()
                .all(voice::VoiceDeck::eligible),
            "no declaration, no narrowing"
        );

        assert!(
            serde_json::from_value::<Vec<voice::VoiceDeckChoice>>(serde_json::json!([
                { "deckId": "deck-local", "eligible": true },
            ]))
            .is_err(),
            "nothing the declaration does not name"
        );
        let many: Vec<voice::VoiceDeckChoice> = (0..=MAX_VOICE_DECK_STEP_ROWS)
            .map(|index| voice::VoiceDeckChoice {
                deck_id: format!("deck-{index}"),
                reason: None,
            })
            .collect();
        assert!(validate_voice_deck_step(&many).is_err());
        let long_reason = [voice::VoiceDeckChoice {
            deck_id: "deck-local".to_string(),
            reason: Some("x".repeat(MAX_VOICE_DECK_REASON_BYTES + 1)),
        }];
        assert!(validate_voice_deck_step(&long_reason).is_err());
        let long_id = [voice::VoiceDeckChoice {
            deck_id: "d".repeat(MAX_VOICE_DECK_ID_BYTES + 1),
            reason: None,
        }];
        assert!(validate_voice_deck_step(&long_id).is_err());
    }

    /// The declaration's wire shape is the webview's: camelCase, and nothing
    /// it does not name.
    #[test]
    fn voice_directory_declarations_deserialize_from_the_webview_shape() {
        let parsed: voice::VoiceDirectories = serde_json::from_value(serde_json::json!({
            "deckId": "deck-1",
            "path": "/home/dev",
            "hasParent": false,
            "entries": [{ "name": "billing", "path": "/home/dev/billing" }],
        }))
        .expect("parses");
        assert_eq!(parsed.deck_id, "deck-1");
        assert!(!parsed.has_parent);
        assert_eq!(parsed.entries[0].name, "billing");
        assert!(
            serde_json::from_value::<voice::VoiceDirectories>(serde_json::json!({
                "deckId": "deck-1",
                "path": "/home/dev",
                "hasParent": false,
                "entries": [],
                "cursor": 3,
            }))
            .is_err()
        );
    }

    /// PRD #1105 M11 step 4: only a window's `Focused` event carries its focus
    /// state, in both directions; any other window event is not a focus change.
    #[test]
    fn only_a_focused_window_event_reports_focus() {
        assert_eq!(window_focus(&tauri::WindowEvent::Focused(true)), Some(true));
        assert_eq!(
            window_focus(&tauri::WindowEvent::Focused(false)),
            Some(false)
        );
        assert_eq!(window_focus(&tauri::WindowEvent::Destroyed), None);
    }

    /// PRD #802's audit blocker: a destroyed window ends capture, and an
    /// ordinary window event does not.
    ///
    /// The half of the classification a test can reach.
    /// `tauri::WindowEvent::CloseRequested` carries a `CloseRequestApi` that
    /// nothing outside tauri can construct, so it is covered by the `matches!`
    /// arm and by review rather than by an assertion here; what this pins is
    /// the direction that would be a bug in either direction — `Destroyed`
    /// must release, and a focus change must not, because focus changes arrive
    /// constantly while a user is dictating.
    #[test]
    fn a_destroyed_window_ends_capture_and_a_focus_change_does_not() {
        assert!(window_ends_capture(&tauri::WindowEvent::Destroyed));
        assert!(!window_ends_capture(&tauri::WindowEvent::Focused(true)));
        assert!(!window_ends_capture(&tauri::WindowEvent::Focused(false)));
    }

    /// A [`VoiceState`] whose device is a stub delivering `seconds` of tone and
    /// whose machine is a stub that grants every inhibit and counts them.
    ///
    /// The second value is the stub stream's own flag, set by its `Drop` — the
    /// only way to assert the DEVICE was released rather than merely that the
    /// state machine says idle. The third is the wake counters, which outlive
    /// every hold made from them and are therefore the only way to assert the
    /// machine was let go of rather than merely that nothing claims otherwise.
    ///
    /// **A stub inhibitor rather than the platform one**, and not only for
    /// determinism: `cargo test-fast` runs this tier dozens of tests at a time
    /// on a developer's own machine, and a test that took a real logind
    /// inhibitor would be a test suite that keeps a laptop awake.
    /// `voice::wake`'s own tests carry the one case that does touch the
    /// platform.
    fn stub_voice_state(
        seconds: f64,
    ) -> (
        VoiceState,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<voice::WakeCounts>,
    ) {
        let source = voice::StubSource::tone(
            voice::AudioFormat::new(voice::TARGET_SAMPLE_RATE, 1),
            seconds,
        );
        let stopped = source.stopped();
        let inhibitor = voice::StubInhibitor::new();
        let counts = inhibitor.counts();
        (
            VoiceState {
                hold: voice::VoiceHold::with_parts(
                    Arc::new(voice::CaptureSession::new(Arc::new(source))),
                    Arc::new(voice::WakeLock::new(Arc::new(inhibitor))),
                ),
            },
            stopped,
            counts,
        )
    }

    /// PRD #802 audit blocker: the app exiting releases the microphone.
    ///
    /// Recording lifetime is Rust's, and before this the only thing that ended
    /// a recording on teardown was a React passive-effect cleanup in the
    /// webview — which a hard reload, a crashed web-content process or a
    /// destroyed window is not guaranteed to run, let alone to complete its
    /// asynchronous IPC. This asserts the device is closed and the captured
    /// audio dropped on the app's own way out, with no webview involved at all.
    #[tokio::test]
    async fn app_exit_releases_the_microphone() {
        let (voice_state, stopped, _counts) = stub_voice_state(1.0);
        let (opened, _ticket) = voice_state.hold.start().expect("the stub device opens");
        assert_eq!(opened.state, voice::CaptureState::Recording);
        assert!(
            voice_state.hold.status().captured_ms > 0,
            "the stub delivered its tone, so there is audio to drop"
        );
        assert!(!stopped.load(Ordering::Relaxed), "the device is still open");

        release_on_exit(&DesktopState::default(), &voice_state).await;

        let after = voice_state.hold.status();
        assert_eq!(
            after.state,
            voice::CaptureState::Idle,
            "app exit must leave no recording behind"
        );
        assert_eq!(
            after.captured_ms, 0,
            "app exit must drop the captured audio"
        );
        assert!(
            stopped.load(Ordering::Relaxed),
            "app exit must close the device stream"
        );
    }

    /// The compounding half of the same blocker: a capped recording.
    ///
    /// [`voice::CaptureSession::cap_reached`] releases the device but
    /// deliberately KEEPS the audio and leaves the session `Recording`, so the
    /// utterance stays the user's to send or discard. Without a Rust-side
    /// teardown, a webview that vanished at that moment left up to 960 KB of
    /// captured speech in the session with nothing able to reach it.
    #[tokio::test]
    async fn app_exit_drops_the_audio_a_capped_recording_kept() {
        let (voice_state, _stopped, _counts) = stub_voice_state(2.0);
        let (_, ticket) = voice_state.hold.start().expect("the stub device opens");
        let capped = voice_state.hold.cap_reached(ticket);
        assert!(capped.capped, "the cap released the device");
        assert_eq!(
            capped.state,
            voice::CaptureState::Recording,
            "the cap keeps the session recording on purpose"
        );
        assert!(capped.captured_ms > 0, "and keeps the audio with it");

        release_on_exit(&DesktopState::default(), &voice_state).await;

        let after = voice_state.hold.status();
        assert_eq!(after.state, voice::CaptureState::Idle);
        assert_eq!(
            after.captured_ms, 0,
            "app exit must drop the audio the cap kept"
        );
    }

    /// PRD #802's sleep work: the app exiting lets the machine sleep again.
    ///
    /// The same blocker as the microphone's, one resource over. An inhibit is
    /// invisible — there is no indicator for it the way the OS shows a live
    /// microphone — so a stuck one is a laptop that never sleeps again with
    /// nothing anywhere to explain why, and the user's remedy is a reboot.
    ///
    /// This is teardown trigger 4. The other three — `CloseRequested` and
    /// `Destroyed`, the document being replaced, and the web-content process
    /// dying on Apple's platforms — reach the same
    /// [`release_microphone`], which is what
    /// [`the_only_teardown_call_releases_both`] pins from the other side.
    #[tokio::test]
    async fn app_exit_lets_the_machine_sleep_again() {
        let (voice_state, _stopped, counts) = stub_voice_state(1.0);
        voice_state.hold.start().expect("the stub device opens");
        assert!(
            voice_state.hold.awake_held(),
            "voice on holds the machine awake"
        );
        assert_eq!(counts.outstanding(), 1);

        release_on_exit(&DesktopState::default(), &voice_state).await;

        assert!(
            !voice_state.hold.awake_held(),
            "app exit must let the machine sleep"
        );
        assert_eq!(
            counts.outstanding(),
            0,
            "and the inhibit must be given back, not merely forgotten"
        );
    }

    /// The other three teardown triggers, at the one function they share.
    ///
    /// `on_window_event`, `on_page_load` and `on_web_content_process_terminate`
    /// each call [`release_microphone`] and nothing else, and none of the three
    /// can be driven from a unit test — they need a built `tauri::App` and a
    /// real event loop. So what is pinned here is the thing they all depend on:
    /// that ONE call releases both the device and the machine.
    ///
    /// The property that no *fourth* path could release the device without the
    /// inhibit is not this test's to make, and it is not made by review either
    /// — `voice::hold`'s privacy makes `CaptureSession::cancel` unreachable
    /// from this file, so `release` is the only cancel that compiles here.
    #[test]
    fn the_only_teardown_call_releases_both() {
        let (voice_state, stopped, counts) = stub_voice_state(1.0);
        voice_state.hold.start().expect("the stub device opens");
        assert!(voice_state.hold.awake_held());

        let after = release_microphone(&voice_state);

        assert_eq!(after.state, voice::CaptureState::Idle);
        assert!(stopped.load(Ordering::Relaxed), "the device is closed");
        assert!(!voice_state.hold.awake_held(), "the machine may sleep");
        assert_eq!(counts.outstanding(), 0);
    }

    /// A machine that refuses the inhibit gets voice control anyway.
    ///
    /// The failure policy, at the level the app sees it: no error crosses the
    /// IPC boundary, no state is left half-set, and the recording is real. The
    /// only observable difference is that `awake_held` says `false` — which is
    /// the honest answer and is reported to nobody, because there is nothing a
    /// user could do about it from inside this app.
    #[test]
    fn a_refused_inhibit_does_not_take_voice_with_it() {
        let source =
            voice::StubSource::tone(voice::AudioFormat::new(voice::TARGET_SAMPLE_RATE, 1), 1.0);
        let voice_state = VoiceState {
            hold: voice::VoiceHold::with_parts(
                Arc::new(voice::CaptureSession::new(Arc::new(source))),
                Arc::new(voice::WakeLock::new(Arc::new(
                    voice::StubInhibitor::refusing(),
                ))),
            ),
        };

        let (opened, _ticket) = voice_state
            .hold
            .start()
            .expect("a machine that will not stay awake still has a microphone");

        assert_eq!(opened.state, voice::CaptureState::Recording);
        assert!(
            voice_status(&voice_state.hold).capture.captured_ms > 0,
            "the utterance is genuinely being captured"
        );
        assert!(!voice_state.hold.awake_held());

        release_microphone(&voice_state);
        assert_eq!(
            voice_state.hold.status().state,
            voice::CaptureState::Idle,
            "and the ordinary teardown still runs"
        );
    }

    /// A settings save that changed no deck must NOT take the switch path
    /// (PRD #741 M9).
    ///
    /// Every settings save reaches `apply_selection`, theme and zoom included,
    /// so this is not an efficiency test: the switch path detaches every
    /// terminal session, and running it unconditionally would tear down a user's
    /// live panes because they changed the colour scheme. The selection
    /// generation is the observable proxy — it is bumped on exactly the path
    /// that also detaches.
    #[tokio::test]
    async fn an_ordinary_settings_save_does_not_retarget_the_deck() {
        // `retarget_selection` writes the process-global applied selection, so
        // every test here that calls it holds this for its duration (issue
        // #1078) — otherwise a sibling's write lands between this one and the
        // `selected_endpoint()` read that decides whether the deck moved.
        let _selection = crate::dto::SELECTION_LOCK.lock().await;
        let state = DesktopState::default();
        let settings = DesktopSettings::default();
        // The selection in force starts as this document's, so the save below is
        // the "changed something else" case.
        retarget_selection(&state, &settings).await;
        let before = *state.selection.borrow();

        assert!(
            !retarget_selection(&state, &settings).await,
            "saving the same document must not read as a deck change"
        );
        assert_eq!(
            *state.selection.borrow(),
            before,
            "the watcher must not be told to re-subscribe, and no session detached"
        );
    }

    /// A document selecting the whole fleet, with `hosts` as its rows — every
    /// one of them connectable, since a row with no socket path is deliberately
    /// not observed.
    fn fleet_of(hosts: &[&str]) -> DesktopSettings {
        use crate::settings::{EndpointId, EndpointSettings, RemoteEndpointSettings, Selection};
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let remote = hosts
            .iter()
            .enumerate()
            .map(|(index, host)| {
                let id = EndpointId::parse(&format!("deck00000000000{index}")).expect("a valid id");
                let mut row =
                    RemoteEndpointSettings::new(id, Hostname::parse(host).expect("a valid host"));
                row.socket = Some(RemoteSocketPath::parse("/run/deck.sock").expect("a path"));
                row
            })
            .collect();
        DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote,
                selection: Selection::All,
            }),
            ..DesktopSettings::default()
        }
    }

    /// A configured deck with no socket path gets NO transport and NO watcher,
    /// however many saves go past it (PRD #742 M12).
    ///
    /// The assertion is on the two sets the live halves are driven from, and it
    /// is the half of this milestone that is easy to get wrong in the other
    /// direction: having made a socketless deck visible, the tempting next move
    /// is to make it observable too, and a watcher against an endpoint that
    /// cannot exist would retry forever against nothing. There is no endpoint to
    /// assert on, which is precisely why this is asserted by COUNT — a
    /// socketless row can contribute no key to either set, because
    /// `EndpointIdentity` is derived from a `RemoteEndpoint` and that row cannot
    /// build one.
    #[tokio::test]
    async fn a_deck_with_no_socket_gets_no_watcher_and_no_tunnel() {
        use crate::settings::{EndpointId, RemoteEndpointSettings};
        use dot_agent_deck::remote_tunnel::Hostname;

        let _selection = crate::dto::SELECTION_LOCK.lock().await;
        let state = DesktopState::default();
        let mut fleet = fleet_of(&["build-box.example.com", "laptop.example.com"]);
        let endpoints = fleet.endpoints.as_mut().expect("the fleet has a section");
        endpoints.remote.push(RemoteEndpointSettings::new(
            EndpointId::parse("halfway").expect("a valid id"),
            Hostname::parse("relay.example.com").expect("a valid host"),
        ));

        assert_eq!(
            fleet.unconfigured_decks().len(),
            1,
            "the state under test: three configured rows, one with no address"
        );
        assert_eq!(
            observed_keys(&fleet).len(),
            3,
            "the local deck and the two rows with an address — and nothing for the third"
        );

        // The watcher fan-out is `ensure_snapshot_watchers`, which iterates
        // `observed_decks()` — so what it would start is exactly what this loop
        // starts, and there is no fourth endpoint for it to reach.
        for endpoint in fleet.connectable_endpoints() {
            state.tunnels.insert_stand_in(&endpoint).await;
            assert!(
                state.start_watcher_once_for(&endpoint.identity()).is_some(),
                "every connectable deck starts unwatched: {endpoint:?}"
            );
        }
        retarget_selection(&state, &fleet).await;

        assert_eq!(
            state.tunnels.held().await,
            3,
            "a save neither builds a transport for the unaddressed deck nor drops one it never had"
        );
        assert_eq!(
            state.watched_decks(),
            observed_keys(&fleet),
            "and the watched set is the connectable set exactly — a watcher for the third \
             deck would be spinning against an address that does not exist"
        );
        assert_eq!(
            state.watched_decks().len(),
            3,
            "three watchers for four fleet members, which is the whole point of the split"
        );
    }

    /// **PRD #742 M14.** Scenario: a deck's watcher task ends — the shape a
    /// panic inside the loop takes — and the deck is then asked for a watcher
    /// again. It must get one.
    ///
    /// # Why a dead claim was worse than no claim
    ///
    /// A watcher loops forever by construction, so its task ending at all is a
    /// bug. What made that bug PERMANENT was the claim outliving it: the slot
    /// stayed in the map, every later `start_watcher_once_for` for that deck
    /// answered `None`, and the deck had no watcher for the life of the
    /// process. Every path that re-runs `ensure_snapshot_watchers` — the
    /// `desktop_bootstrap` the webview's Reconnect reaches, a settings save's
    /// `apply_selection`, and the two `desktop_run_action` arms that
    /// re-bootstrap — goes through that same refusal, so every remedy a user
    /// could reach for did nothing.
    ///
    /// # Why M14 is where it gets fixed
    ///
    /// A deck with no watcher emits no snapshot. Before M14 that deck was
    /// simply absent from the fleet view, which is wrong quietly; now it is a
    /// group saying it is being waited for, which is wrong loudly and forever.
    /// Bounding the pending state means the paths that produce a snapshot are
    /// bounded AND the thing that produces them can be restarted.
    ///
    /// The claim handed out afterwards carries a NEW token, which is what keeps
    /// the dead task's own `register_watcher` — if it is still in flight —
    /// from writing its handle into the live claim.
    #[tokio::test]
    async fn a_watcher_whose_task_has_ended_does_not_hold_the_deck_hostage() {
        let _selection = crate::dto::SELECTION_LOCK.lock().await;
        let state = DesktopState::default();
        let fleet = fleet_of(&["build-box.example.com"]);
        let deck = fleet
            .connectable_endpoints()
            .into_iter()
            .next()
            .expect("the fleet has a deck")
            .identity();

        let first = state
            .start_watcher_once_for(&deck)
            .expect("an unwatched deck hands out a claim");
        assert!(
            state.start_watcher_once_for(&deck).is_none(),
            "a live claim refuses a second watcher, which is the whole point of the claim"
        );

        /*
            A task that RETURNS stands in for one that panicked: `is_finished`
            is true for both, and a panicking task would take the test binary's
            runtime with it in a way a test cannot read back. It is registered
            while still RUNNING, which is the ordering the real watcher has —
            registering an already-finished handle would prove nothing about the
            window this is really about.
        */
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let ending = tauri::async_runtime::spawn(async move {
            let _ = release_rx.await;
        });
        state.register_watcher(&deck, first, ending);
        assert!(
            state.start_watcher_once_for(&deck).is_none(),
            "and a REGISTERED, still-running watcher refuses one too — an absent \
             handle and a finished one must not read the same"
        );

        let _ = release.send(());
        // Bounded by TIME, and by a sleep that actually parks this thread —
        // never by an iteration count on this runtime.
        //
        // `tauri::async_runtime::spawn` does not put the task on the runtime
        // `#[tokio::test]` built. With no `async_runtime::set` anywhere in this
        // binary it lands on tauri's global `default_runtime()`, a
        // multi-threaded tokio runtime with its own OS threads, so the release
        // has to travel across a runtime boundary: a worker thread there must be
        // scheduled, poll the released task to completion, and only then does
        // `is_finished` flip for the handle `watching` reads. `yield_now` here
        // reschedules only this test's own task and hands that worker nothing.
        //
        // So the old `for _ in 0..1_000 { yield_now().await }` was a ~4 ms wall
        // clock budget wearing an iteration count: measured on an idle 16-core
        // Linux box it resolved in 1–11 iterations and 17–41 us. Under 4x CPU
        // oversubscription the same loop needed 46–174 iterations and exhausted
        // all 1000 once in ten runs — reproducing, on Linux, the intermittent
        // `build-windows` failure that sent it here. Windows loses it far more
        // readily: its scheduling quantum alone is ~15.6 ms, four times the
        // budget the loop actually had.
        const RECLAIM_TIMEOUT: Duration = Duration::from_secs(5);
        const RECLAIM_POLL: Duration = Duration::from_millis(5);
        let reclaimed = tokio::time::timeout(RECLAIM_TIMEOUT, async {
            loop {
                if let Some(token) = state.start_watcher_once_for(&deck) {
                    return token;
                }
                tokio::time::sleep(RECLAIM_POLL).await;
            }
        })
        .await;

        let second =
            reclaimed.expect("a deck whose watcher task has ended must be able to get another");
        assert_ne!(
            second, first,
            "and under a NEW token, so the dead task's own register_watcher cannot \
             write its handle into the live claim"
        );

        crate::dto::apply_settings_selection(&DesktopSettings::default());
    }

    /// The live set `retain` is given names every observed deck, not the one
    /// the deck screen resolves to (PRD #742 M2).
    ///
    /// The seam this milestone is: `retain` and its map were already keyed and
    /// already took a set, and the single-deck thing was this caller building a
    /// one-element one. Asserted on keys rather than through `retarget_selection`
    /// because acquiring a remote deck's transport would spawn `ssh`; that the
    /// keys then keep their tunnels is `endpoint_tunnels`' own pair of tests.
    #[test]
    fn the_fleets_live_set_names_every_observed_deck() {
        use dot_agent_deck::daemon_client::Endpoint;

        let fleet = fleet_of(&["build-box.example.com", "laptop.example.com"]);
        let keys = observed_keys(&fleet);
        assert_eq!(keys.len(), 3, "the local deck plus both configured rows");
        assert!(keys.contains(&Endpoint::local().identity()));
        for endpoint in fleet.connectable_endpoints() {
            assert!(keys.contains(&endpoint.identity()), "{endpoint:?}");
        }

        // Every other selection retains over precisely the deck it resolves to,
        // which is the set this caller built before M2 and still builds.
        let single = DesktopSettings::default();
        assert_eq!(
            observed_keys(&single),
            [single.resolve_endpoint().endpoint.identity()]
                .into_iter()
                .collect()
        );
    }

    /// Whatever the selection, the deck the screen is talking to is one the
    /// fleet observes — so `retain` can never release the transport under the
    /// deck screen's own terminals (PRD #742 M2).
    #[test]
    fn the_deck_the_screen_talks_to_is_always_one_the_fleet_observes() {
        use crate::settings::{EndpointId, EndpointSettings, Selection};

        let fleet = fleet_of(&["build-box.example.com", "laptop.example.com"]);
        let endpoints = fleet.endpoints.clone().expect("the fleet has a section");
        let selections = [
            Selection::All,
            Selection::Local,
            Selection::One(endpoints.remote[1].id.clone()),
            // The two selections that fall back: a row that is gone, and — since
            // `fleet_of` gives every row a socket — a hand-written id that never
            // named one. Both resolve to the local deck, which leads every set.
            Selection::One(EndpointId::parse("deck0000000000ff").expect("a valid id")),
        ];
        for selection in selections {
            let settings = DesktopSettings {
                endpoints: Some(EndpointSettings {
                    selection: selection.clone(),
                    ..endpoints.clone()
                }),
                ..DesktopSettings::default()
            };
            assert!(
                observed_keys(&settings).contains(&settings.resolve_endpoint().endpoint.identity()),
                "the resolved deck must be observed under {selection:?}"
            );
        }
        // And with no `[endpoints]` section at all, which resolves and observes
        // the local deck without either method reading a row.
        let bare = DesktopSettings::default();
        assert!(observed_keys(&bare).contains(&bare.resolve_endpoint().endpoint.identity()));
    }

    /// With the fleet selected, a save keeps EVERY observed deck's transport;
    /// with one deck selected, the same save keeps one (PRD #742 M2).
    ///
    /// The end-to-end version of the milestone, at the caller that was the
    /// single-deck half: `retain` and its map were already keyed by
    /// `EndpointIdentity` and already took a set, and what made the app
    /// single-deck was this function building a one-element one from
    /// `resolve()`. A remote deck's transport is seeded rather than acquired
    /// because acquiring one spawns `ssh`; `retain` never reads a connection,
    /// so the seam fabricates nothing the assertion rests on.
    #[tokio::test]
    async fn the_fleet_keeps_every_observed_decks_transport_and_one_selection_keeps_one() {
        use crate::settings::{EndpointSettings, Selection};

        let _selection = crate::dto::SELECTION_LOCK.lock().await;
        let state = DesktopState::default();
        let fleet = fleet_of(&["build-box.example.com", "laptop.example.com"]);
        let observed = fleet.connectable_endpoints();
        assert_eq!(observed.len(), 3, "the local deck plus both rows");
        for endpoint in &observed {
            state.tunnels.insert_stand_in(endpoint).await;
        }

        retarget_selection(&state, &fleet).await;
        assert_eq!(
            state.tunnels.held().await,
            3,
            "every deck the fleet observes keeps its transport across a save"
        );

        // The same document, now naming one deck. The other two leave the
        // observed set and exactly they are dropped.
        let endpoints = fleet.endpoints.clone().expect("the fleet has a section");
        let one = DesktopSettings {
            endpoints: Some(EndpointSettings {
                selection: Selection::One(endpoints.remote[0].id.clone()),
                ..endpoints
            }),
            ..DesktopSettings::default()
        };
        retarget_selection(&state, &one).await;

        assert_eq!(state.tunnels.held().await, 1);
        // Which one survived, asserted through `release` rather than `acquire`:
        // acquiring a remote deck that had wrongly been dropped would leave the
        // test spawning `ssh` at a hostname that does not resolve, so the one
        // assertion that could hang is the one not made here.
        state
            .tunnels
            .release(&one.resolve_endpoint().endpoint)
            .await;
        assert_eq!(
            state.tunnels.held().await,
            0,
            "the deck still held is the deck still selected"
        );
        crate::dto::apply_settings_selection(&DesktopSettings::default());
    }

    /// Adding a deck to the fleet is not the deck screen moving, so it must not
    /// take the switch path (PRD #742 M2).
    ///
    /// The set-level generalisation of `an_ordinary_settings_save_does_not_retarget_the_deck`,
    /// and the reason the gate stays on `resolve()`'s key rather than on the
    /// observed set: under `Selection::All` the screen and its terminals are on
    /// the local deck whatever the fleet gains or loses, so widening the gate to
    /// "the set changed" would detach a user's live panes because they added a
    /// row in the settings sheet. The tunnel the local deck already holds
    /// survives with them — `retain` over the grown set still names it.
    #[tokio::test]
    async fn growing_the_fleet_does_not_retarget_the_deck_screen() {
        let _selection = crate::dto::SELECTION_LOCK.lock().await;
        let state = DesktopState::default();
        let one = fleet_of(&["build-box.example.com"]);
        retarget_selection(&state, &one).await;
        let before = *state.selection.borrow();
        let local = dot_agent_deck::daemon_client::Endpoint::local();
        let lease = state
            .tunnels
            .acquire(&local)
            .await
            .expect("lease the local deck");

        let grown = fleet_of(&["build-box.example.com", "laptop.example.com"]);
        assert!(
            !retarget_selection(&state, &grown).await,
            "a deck joining the fleet is not the deck screen moving"
        );

        assert_eq!(
            *state.selection.borrow(),
            before,
            "no session detached, and the watcher was not told to re-subscribe"
        );
        assert!(
            std::sync::Arc::ptr_eq(
                &lease,
                &state.tunnels.acquire(&local).await.expect("still leased")
            ),
            "the deck screen's own transport survives a fleet edit"
        );
        crate::dto::apply_settings_selection(&DesktopSettings::default());
    }

    // -----------------------------------------------------------------------
    // PRD #742 M3 — the watcher set
    //
    // M2 left exactly this half open and said so: `retain` already tears down a
    // departed deck's TRANSPORT, and a set difference is all a teardown needs —
    // but a set that GAINED a member needs a watcher STARTED, which is the one
    // set-level event a difference cannot be read backwards from, and M2 did not
    // derive it because it had no consumer. The watchers are the consumer.
    // -----------------------------------------------------------------------

    /// **Test-plan item 11.** Scenario: three decks are observed and each has
    /// claimed a watcher; the settings document is then saved with one deck
    /// removed. After the save the departed deck is no longer watched, the two
    /// that remain still are, and re-adding the departed deck hands out a fresh
    /// watcher for it.
    ///
    /// Pinned at `retarget_selection` rather than at the registry alone, because
    /// that is the function every settings save already reaches and where the
    /// transport half of the same teardown already lives — one line below
    /// `state.tunnels.retain(&observed_keys(settings))`. A watcher left running
    /// for a deck the user has dropped goes on emitting that deck's records into
    /// a view that no longer has a group for them.
    ///
    /// **What this does NOT pin**, stated because the ordering is the
    /// load-bearing half of item 11 and this cannot reach it: that the watcher
    /// ends BEFORE any further record from that deck is emitted. Emission needs
    /// an `AppHandle`, which needs a running Tauri app. #741's own answer to the
    /// same problem is the shape to copy — `watch_one_subscription` returns on
    /// the selection arm *before* its refresh, so the stale fold is discarded
    /// with the subscription that filled it rather than answering one more
    /// snapshot.
    #[tokio::test]
    async fn a_deck_that_leaves_the_fleet_stops_being_watched() {
        let _selection = crate::dto::SELECTION_LOCK.lock().await;
        let state = DesktopState::default();
        let fleet = fleet_of(&["build-box.example.com", "laptop.example.com"]);
        for endpoint in fleet.connectable_endpoints() {
            assert!(
                state.start_watcher_once_for(&endpoint.identity()).is_some(),
                "every observed deck starts unwatched: {endpoint:?}"
            );
        }
        assert_eq!(state.watched_decks(), observed_keys(&fleet));

        let smaller = fleet_of(&["build-box.example.com"]);
        let kept = observed_keys(&smaller);
        let departed: Vec<_> = observed_keys(&fleet).difference(&kept).cloned().collect();
        assert_eq!(departed.len(), 1, "exactly one deck leaves the fleet");
        let departed = departed.into_iter().next().expect("the departed deck");

        retarget_selection(&state, &smaller).await;

        assert!(
            !state.watched_decks().contains(&departed),
            "a deck dropped from the observed set must stop being watched"
        );
        assert_eq!(
            state.watched_decks(),
            kept,
            "and the decks that stayed must keep the watchers they had"
        );
        assert!(
            state.start_watcher_once_for(&departed).is_some(),
            "a deck that rejoins the fleet needs a watcher started again, which \
             is the half a set difference cannot be read backwards from"
        );

        crate::dto::apply_settings_selection(&DesktopSettings::default());
    }

    /// **PRD #742 M3's deck stamp, at the one layer a test can reach.** Scenario:
    /// a hook event from a named deck is turned into a `desktop://daemon-event`
    /// payload. The payload must carry `deck`, and must leave `kind` and every
    /// snake_case `AgentEvent` field exactly where they were.
    ///
    /// The stamp exists because `BroadcastMsg` carries no deck id and this event
    /// was emitted unwrapped, so with N watchers "which deck is this from" had no
    /// answer at all. It is added by `#[serde(flatten)]` rather than by wrapping
    /// precisely so that `desktop/src/lib/daemonEvents.ts` needs no change —
    /// the frontend is M4's — and that is the claim this test exists to check,
    /// because a wrapper would break the evidence drawer and the handoff edges
    /// silently and nothing else here would notice.
    ///
    /// **It does not reach the emit**, which is the residual: `app.emit` needs an
    /// `AppHandle` and therefore a running Tauri app, and making the chain
    /// generic over `R: Runtime` to reach one from `MockRuntime` is a production
    /// refactor M3 deliberately did not start. So what stays unguarded is that
    /// the watcher passes its **own** endpoint here — check that by hand with two
    /// decks observed, where every payload's `deck` must equal the
    /// `connection.deckId` of the deck that emitted it.
    ///
    /// **PRD #742 M5 changed the stamp's value from the label to the key**, and
    /// the assertion below moved with it: the webview compares this against the
    /// deck id it holds the group under, and a stamp that stayed
    /// `Endpoint::describe()` would have gone on collapsing two daemons on one
    /// host into one answer after the snapshot stopped doing so.
    #[test]
    fn a_stamped_daemon_event_adds_the_deck_and_moves_nothing_else() {
        use dot_agent_deck::daemon_client::{Endpoint, LocalEndpoint};
        use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};

        let event = BroadcastMsg::Event(AgentEvent {
            session_id: "pane-a-session".into(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            tool_name: Some("Bash".into()),
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: std::collections::HashMap::new(),
            pane_id: Some("pane-a".into()),
            agent_id: Some("agent-a".into()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        });
        let deck = Endpoint::Local(LocalEndpoint::at("/run/deck-a.sock"));

        let bare = serde_json::to_value(&event).expect("the unstamped payload");
        let stamped =
            serde_json::to_value(DeckStamped::new(&deck, &event)).expect("the stamped payload");

        assert_eq!(
            stamped["deck"],
            serde_json::Value::from(crate::dto::deck_wire_id(&deck)),
            "the payload must name the deck it came from, and with the same \
             token `connection.deckId` carries"
        );
        assert_ne!(
            stamped["deck"], "/run/deck-a.sock",
            "the stamp is the KEY, not the label — a deck path here is the \
             identity that cannot tell two daemons on one host apart"
        );
        assert_eq!(
            stamped["kind"], "event",
            "the internally-tagged discriminator must survive the flatten — a \
             wrapper here would strand every reader in daemonEvents.ts"
        );
        assert_eq!(stamped["tool_name"], "Bash", "snake_case fields stay put");
        assert_eq!(stamped["agent_id"], "agent-a");

        // Additive, stated as a set difference rather than field by field: the
        // frontend is M4's, so this milestone may ADD to this payload and may
        // move nothing already in it.
        let bare_keys = bare.as_object().expect("a map").clone();
        let stamped_keys = stamped.as_object().expect("a map").clone();
        for (key, value) in &bare_keys {
            assert_eq!(
                stamped_keys.get(key),
                Some(value),
                "the stamp moved `{key}`, which no frontend change accompanies"
            );
        }
        let added: Vec<_> = stamped_keys
            .keys()
            .filter(|key| !bare_keys.contains_key(*key))
            .collect();
        assert_eq!(added, vec!["deck"], "exactly one field is added");
    }

    /// Scenario: one deck's watcher is claimed twice, then the fleet is emptied
    /// and the deck is claimed again. A deck that is already watched must not
    /// hand out a second watcher, and a deck whose watcher was ended must hand
    /// out a fresh one.
    ///
    /// The N-deck form of `DesktopState::start_watcher_once`, whose `AtomicBool`
    /// is the single-deck version of exactly this: one claim per deck rather
    /// than one claim per process. Two watchers on one deck would fold the same
    /// broadcast twice and emit two snapshots per coalescing window for it.

    #[test]
    fn each_observed_deck_claims_exactly_one_watcher() {
        use dot_agent_deck::daemon_client::Endpoint;

        let state = DesktopState::default();
        let local = Endpoint::local().identity();

        assert!(
            state.start_watcher_once_for(&local).is_some(),
            "the first claim starts it"
        );
        assert!(
            state.start_watcher_once_for(&local).is_none(),
            "a deck that is already watched must not start a second watcher"
        );

        state.retain_watchers(&std::collections::HashSet::new());
        assert!(
            state.watched_decks().is_empty(),
            "a deck named by no observed set keeps no watcher"
        );
        assert!(
            state.start_watcher_once_for(&local).is_some(),
            "a deck whose watcher was ended must be startable again"
        );
    }

    /// **PRD #742 M8's R2.** Scenario: watcher A claims the local deck's slot,
    /// and before it can register its handle the deck leaves the observed set
    /// and rejoins — so watcher B claims a fresh slot for the same deck. A's
    /// handle then arrives. It must be ABORTED, because A is watching under a
    /// claim nobody holds any more; and B's must be stored, because B is the
    /// watcher the registry is now tracking.
    ///
    /// Before the claim carried a token, `register_watcher` could only ask "is
    /// there a claim here". A's handle went into B's slot, B's own registration
    /// then *replaced* it, and replacing a `JoinHandle` drops it rather than
    /// aborting it — so A ran untracked and unstoppable for the life of the
    /// process, double-folding every broadcast for that deck.
    ///
    /// **What this proves:** the registry's decision, for the exact ordering the
    /// reviewer described. Abort is observed rather than assumed — each task
    /// owns a `oneshot::Sender` it never sends on, so the receiver resolves
    /// (with a closed-channel error) exactly when the task's future is dropped,
    /// which for a parked `pending()` means it was aborted. The second half is
    /// the one that fails if `register_watcher` simply aborted everything: B's
    /// handle has to still be in the slot for `retain_watchers` to end it.
    ///
    /// **What it does not prove:** that the interleaving is reachable from the
    /// real callers. The reviewer rated that low and could not construct an
    /// ordering — `spawn_deck_watcher` runs claim, spawn and register with no
    /// `.await` between the two lock acquisitions. The claim here is that the
    /// registry is correct if it ever happens, not that it does.
    #[tokio::test]
    async fn a_watcher_handle_arriving_for_someone_elses_claim_is_aborted() {
        use dot_agent_deck::daemon_client::Endpoint;

        let state = DesktopState::default();
        let deck = Endpoint::local().identity();

        let a = state
            .start_watcher_once_for(&deck)
            .expect("watcher A claims the slot");
        // The deck leaves the observed set and rejoins, all before A registers.
        state.retain_watchers(&std::collections::HashSet::new());
        let b = state
            .start_watcher_once_for(&deck)
            .expect("watcher B claims the slot the deck's return re-created");
        assert_ne!(a, b, "a re-claim must not mint the token it replaced");

        // A task that never finishes on its own, and whose `Sender` is therefore
        // dropped only when the task's future is dropped — i.e. when it is
        // aborted.
        let parked = |tx: tokio::sync::oneshot::Sender<()>| {
            tauri::async_runtime::spawn(async move {
                let _tx = tx;
                std::future::pending::<()>().await;
            })
        };

        let (tx_a, rx_a) = tokio::sync::oneshot::channel::<()>();
        state.register_watcher(&deck, a, parked(tx_a));
        tokio::time::timeout(Duration::from_secs(5), rx_a)
            .await
            .expect(
                "watcher A's handle arrived for a claim that is no longer its own, so it must                  be aborted rather than stored — a dropped JoinHandle leaves the task running                  with nothing able to stop it",
            )
            .expect_err("the parked task never sends; the channel closes because it was dropped");

        // And the other half: B's handle went into B's slot, so the registry can
        // still end it. Without this, a `register_watcher` that aborted every
        // arrival would pass the assertion above.
        let (tx_b, rx_b) = tokio::sync::oneshot::channel::<()>();
        state.register_watcher(&deck, b, parked(tx_b));
        state.retain_watchers(&std::collections::HashSet::new());
        tokio::time::timeout(Duration::from_secs(5), rx_b)
            .await
            .expect("watcher B's handle must be held by the registry, so retain_watchers ends it")
            .expect_err("the parked task never sends");
    }

    /// A remote deck at `host` whose daemon listens on `socket` over there,
    /// with every optional field left off.
    fn deck_at(host: &str, socket: &str) -> dot_agent_deck::daemon_client::RemoteEndpoint {
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};
        dot_agent_deck::daemon_client::RemoteEndpoint::new(
            Hostname::parse(host).expect("a valid hostname"),
            RemoteSocketPath::parse(socket).expect("a valid remote socket path"),
        )
    }

    /// The decision is made on the RESOLVED endpoint key, not on the stored
    /// token (PRD #741 M9).
    #[test]
    fn a_deck_moves_when_its_address_moves_even_if_the_token_does_not() {
        use dot_agent_deck::daemon_client::{Endpoint, LocalEndpoint};

        // The key both `DaemonLinks` and `EndpointTunnels` are indexed by. An
        // edit to the selected deck's address moves the tunnel, the link and
        // every terminal on it, while leaving the stored token identical — so a
        // token comparison would miss exactly the case that matters most.
        let box_22 = Endpoint::Remote(deck_at("build-box.example.com", "/run/deck.sock"));
        let box_2222 =
            Endpoint::Remote(deck_at("build-box.example.com", "/run/deck.sock").with_port(2222));
        let local = Endpoint::Local(LocalEndpoint::at("/run/deck.sock"));
        assert!(selection_moved(&box_22.identity(), &box_2222.identity()));
        assert!(!selection_moved(&local.identity(), &local.identity()));
        // A selection that falls back to local compares as local, which is what
        // the app is actually talking to.
        assert!(selection_moved(&box_22.identity(), &local.identity()));
    }

    /// The emit must not be reached with a selection change already pending
    /// (PRD #741, Greptile P1 on #1035).
    ///
    /// The composition — that this is polled immediately before
    /// `snapshot_with` — is read rather than asserted, because the emit needs
    /// an `AppHandle` and therefore a running Tauri app. What is pinned here is
    /// the decision the call site cannot show: a change from ANY point since
    /// the last observation counts, and a dead sender is not one.
    #[test]
    fn a_selection_change_is_seen_after_the_coalescing_sleep_too() {
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        assert!(
            !selection_moved_since_last_seen(&rx),
            "a fresh receiver has seen the generation in force"
        );

        // What the M9 `select!` arm cannot see: the change lands while the loop
        // is in `drain_pending` or the coalescing sleep rather than parked in
        // the arm.
        tx.send(1).expect("the receiver is alive");
        assert!(
            selection_moved_since_last_seen(&rx),
            "the emit must not pair the new deck with the old fold"
        );

        // Observing it clears it, so one change ends one subscription.
        let mut rx = rx;
        rx.mark_unchanged();
        assert!(!selection_moved_since_last_seen(&rx));

        // Every sender gone is not a selection change. Taking it as one would
        // spin the refresh loop instead of coalescing it.
        drop(tx);
        assert!(
            !selection_moved_since_last_seen(&rx),
            "a dead sender must not read as a deck switch"
        );
    }

    /// The three fields `Endpoint::describe()` does not render (PRD #741,
    /// Greptile P1 on #1035).
    ///
    /// Each names a different daemon, or a different ssh route to one, while
    /// leaving `user@host[:port]` byte-identical — so while the comparison was
    /// a display string, editing any of them on the selected deck took the
    /// no-op path: nothing detached, no tunnel was released, and the held link
    /// kept talking through the route the user had just replaced.
    #[test]
    fn a_deck_moves_when_a_field_describe_does_not_render_moves() {
        use dot_agent_deck::daemon_client::Endpoint;
        use dot_agent_deck::remote_tunnel::{HostAlias, KeyPath};

        let base = deck_at("build-box.example.com", "/run/deck.sock");
        let others = [
            // A different daemon on the same host.
            deck_at("build-box.example.com", "/run/other.sock"),
            // A different key, so a different ssh identity.
            base.clone()
                .with_key(KeyPath::parse("~/.ssh/id_ed25519").expect("key path")),
            // A different route to the same host.
            base.clone()
                .with_jump(HostAlias::parse("bastion").expect("jump alias")),
        ];
        for other in others {
            assert_eq!(
                base.describe(),
                other.describe(),
                "the premise of this test is that `describe()` cannot tell these apart"
            );
            assert!(
                selection_moved(
                    &Endpoint::Remote(base.clone()).identity(),
                    &Endpoint::Remote(other.clone()).identity()
                ),
                "a field that changes the connection must move the deck: {other:?}"
            );
        }
    }

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

    /// A start the deck ANSWERED with a refusal: its outcome is known, so the
    /// launch may report it as definitive (PRD #1223 audit V5).
    fn refused(message: &str) -> Result<String, RoleStartFailure> {
        Err(RoleStartFailure {
            message: message.to_string(),
            indeterminate: false,
        })
    }

    /// A start whose reply never arrived: the deck may have received it, so the
    /// launch may not report "nothing started" (PRD #1223 audit V5).
    fn lost(message: &str) -> Result<String, RoleStartFailure> {
        Err(RoleStartFailure {
            message: message.to_string(),
            indeterminate: true,
        })
    }

    struct FakeWorkflowDaemon {
        now: Mutex<std::time::Instant>,
        prepare_requests: Mutex<Vec<PrepareRequest>>,
        prepare_results: Mutex<VecDeque<Result<PreparedWorkflow, String>>>,
        /// Every spawn this fake was asked for, in order and by role name.
        /// `started` records the options; this records the sequence, which is
        /// what a rollback assertion needs to say WHICH role a launch died on.
        spawn_log: Mutex<Vec<String>>,
        start_tokens: Mutex<Vec<Option<String>>>,
        started: Mutex<Vec<StartAgentOptions>>,
        start_results: Mutex<VecDeque<Result<String, RoleStartFailure>>>,
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
        /// PRD #1223 M6: the client library withholds the configured start —
        /// the deck does not advertise `prepared-role-command`.
        configured_unsupported: AtomicBool,
        /// The type the deck reports for a started agent (a configured role's
        /// resolved type), and the ids it was asked about.
        launched_type: Mutex<Option<AgentType>>,
        launched_type_queries: Mutex<Vec<String>>,
        /// PRD #1223 audit W6: how long the deck takes to answer a
        /// preparation. `None` is the ordinary immediate answer.
        prepare_delay: Mutex<Option<Duration>>,
        /// PRD #1223 audit F4: the starts (by position, from 0) the deck
        /// records — the spawn happened — and then never answers.
        start_hangs: Mutex<HashSet<usize>>,
        /// Every stop asked for, answered or not; `stopped` is only the ones
        /// that were confirmed.
        stop_attempts: Mutex<Vec<String>>,
        /// Stops the deck never answers, and stops it refuses, by agent id.
        stop_hangs: Mutex<HashSet<String>>,
        stop_errors: Mutex<HashMap<String, String>>,
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
                spawn_log: Mutex::new(Vec::new()),
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
                configured_unsupported: AtomicBool::new(false),
                launched_type: Mutex::new(None),
                launched_type_queries: Mutex::new(Vec::new()),
                prepare_delay: Mutex::new(None),
                start_hangs: Mutex::new(HashSet::new()),
                stop_attempts: Mutex::new(Vec::new()),
                stop_hangs: Mutex::new(HashSet::new()),
                stop_errors: Mutex::new(HashMap::new()),
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
            // Read and released before the await: the guard is not `Send`, and
            // holding it across one would make this future unspawnable.
            let delay = *self.prepare_delay.lock().unwrap();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            self.prepare_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(prepared_workflow()))
        }

        async fn start_workflow_agent(
            &self,
            options: StartAgentOptions,
            prep_token: Option<&str>,
        ) -> Result<String, RoleStartFailure> {
            self.spawn_log.lock().unwrap().push(format!(
                "start:{}",
                options.display_name.as_deref().unwrap_or("?")
            ));
            self.start_tokens
                .lock()
                .unwrap()
                .push(prep_token.map(str::to_string));
            let (index, agent_id) = {
                let mut started = self.started.lock().unwrap();
                let index = started.len();
                started.push(options);
                (index, format!("agent-{index}"))
            };
            let hangs = self.start_hangs.lock().unwrap().contains(&index);
            if hangs {
                std::future::pending::<()>().await;
            }
            self.start_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(agent_id))
        }

        async fn start_configured_role(
            &self,
            options: StartAgentOptions,
            prep_token: &str,
        ) -> Result<GatedQuery<String>, RoleStartFailure> {
            if self.configured_unsupported.load(Ordering::SeqCst) {
                return Ok(GatedQuery::Unsupported);
            }
            self.start_workflow_agent(options, Some(prep_token))
                .await
                .map(GatedQuery::Answered)
        }

        async fn launched_agent_type(&self, agent_id: &str) -> Result<Option<AgentType>, String> {
            self.launched_type_queries
                .lock()
                .unwrap()
                .push(agent_id.to_string());
            Ok(self.launched_type.lock().unwrap().clone())
        }

        async fn stop_workflow_agent(&self, agent_id: &str) -> Result<(), String> {
            self.stop_attempts
                .lock()
                .unwrap()
                .push(agent_id.to_string());
            let hangs = self.stop_hangs.lock().unwrap().contains(agent_id);
            if hangs {
                std::future::pending::<()>().await;
            }
            let refusal = self.stop_errors.lock().unwrap().get(agent_id).cloned();
            if let Some(refusal) = refusal {
                return Err(refusal);
            }
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

    /// PRD #819 audit follow-up. Scenario: prepare a workflow, then launch it
    /// against a daemon that does not know the prepared-start verb — it answers
    /// the structured `malformed request: unknown variant
    /// \`start-prepared-agent\`` such a daemon replies with, on the spawn's own
    /// connection. The launch must fail closed: **nothing started at all**,
    /// nothing to roll back, and no coordinator briefed.
    ///
    /// This replaces the peer re-verification the preceding round added. That
    /// checked the daemon on a SEPARATE connection immediately before each
    /// prepared spawn, which narrowed the substitution window to the gap between
    /// two `connect()` calls but could not shut it. The verb shuts it: there is
    /// no window, because the refusal arrives on the connection the spawn would
    /// have used.
    ///
    /// The refusal lands on the FIRST role deliberately, which is what makes
    /// this a different assertion from
    /// `lost_start_response_reconciles_failed_pane_before_rollback` — that one
    /// dies on role two and pins the ROLLBACK; this one pins that a refused
    /// launch can leave nothing behind to roll back.
    ///
    /// **Honest about what it discriminates:** the desktop reaches the daemon
    /// through `WorkflowDaemon`, which hides the op, so this test would also
    /// pass before the verb existed with a different scripted string. It is here
    /// to name the scenario at the layer a user meets it. The evidence that the
    /// op actually changed is `the_client_routes_a_presented_token_onto_the_prepared_verb`
    /// and `a_prepared_start_is_a_distinct_op_an_older_daemon_refuses_closed`
    /// in the root crate, each verified by removing its fix.
    #[tokio::test]
    async fn a_daemon_without_the_prepared_verb_fails_the_launch_closed() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            [Ok(SendResult::Applied)],
            Ok(SendResult::Applied),
        );
        daemon.start_results.lock().unwrap().push_back(refused(
            "malformed request: unknown variant `start-prepared-agent`, expected one of \
             `list-agents`, `start-agent`, `hello`",
        ));
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

        let failure = launch_workflow(
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
        let error = &failure.message;

        assert!(
            error.contains("start-prepared-agent"),
            "the daemon's own refusal must survive to the user: {error}"
        );
        // Exactly one spawn was ATTEMPTED and it was refused — which is what
        // makes this the daemon's answer rather than this client declining to
        // ask — and the second role was never reached. (`started` records
        // attempts, not successes: the fake logs the options before consulting
        // its scripted result, which is what
        // `lost_start_response_reconciles_failed_pane_before_rollback` relies on
        // to find the failed role's pane.)
        assert_eq!(*daemon.spawn_log.lock().unwrap(), ["start:planner"]);
        assert_eq!(daemon.started.lock().unwrap().len(), 1);
        assert!(
            daemon.stopped.lock().unwrap().is_empty(),
            "the one attempt was refused, so no role is live and there is nothing to roll back"
        );
        assert!(
            daemon.submissions.lock().unwrap().is_empty(),
            "a launch that failed closed must not brief a coordinator"
        );
    }

    /// The token-LESS counterpart, and the half that keeps the fix from becoming
    /// a tax on every other path: a launch with no preparation sends no token,
    /// so it stays on plain `start-agent` and nothing about it changed.
    #[tokio::test]
    async fn a_token_less_launch_presents_no_token() {
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

        assert_eq!(*daemon.start_tokens.lock().unwrap(), [None, None]);
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

    /// PRD #1223 audit V1, guarded (audit W6): the preparation is under NO
    /// client-side deadline, and nothing else in the suite would notice if one
    /// were put back.
    ///
    /// Every other deck call a launch makes is bounded at
    /// [`WORKFLOW_ROLE_START_TIMEOUT`], and the reason this one is not is
    /// integrity rather than patience: the deck resolves, composes, issues the
    /// token and publishes `orchestrator-context.md` on its blocking pool, and
    /// dropping the client's future stops none of that — so a preparation
    /// reported here as timed out could still publish afterwards, over a
    /// retry's context once the retry's last prepared-role check had passed.
    ///
    /// The clock is paused, so a deck that takes four times the role-start
    /// bound to answer costs the test nothing and would trip any `timeout`
    /// wrapped around this call.
    #[tokio::test(start_paused = true)]
    async fn a_slow_preparation_is_waited_out_rather_than_timed_out() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );
        *daemon.prepare_delay.lock().unwrap() = Some(WORKFLOW_ROLE_START_TIMEOUT * 4);

        let (roles, prepared) = prepare_workflow_launch(
            &daemon,
            "loop",
            "/home/dev/repo",
            "Build it.",
            &launch_roles("claude"),
            None,
        )
        .await
        .expect("a preparation the deck answers late must still be accepted");

        assert!(!prepared.path.is_empty());
        assert!(!roles.is_empty());
        assert_eq!(daemon.prepare_requests.lock().unwrap().len(), 1);
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

    /// A publish refusal reaches the caller **whole** — the path, the mode and
    /// the `chmod` command the daemon composed (issue #1047 §2).
    ///
    /// This is the client half of that fix and the reason it needs a guard of
    /// its own. The daemon has always logged a message naming the mode and the
    /// remedy; what the desktop showed was the shorter sentence that crossed the
    /// wire, so a user saw "remove those write bits and retry" with no path and
    /// no command. The daemon now sends the long one, and nothing between the
    /// socket and the toast may shorten it again — `safe_message` bounds the
    /// string at 2048 characters, which this sits far inside, and a future
    /// tightening of that bound would fail here rather than silently re-open the
    /// defect that cost three launches and a filesystem-wide `find`.
    #[tokio::test]
    async fn a_publish_refusal_reaches_the_caller_with_its_path_and_remedy_intact() {
        let sentence = "publish-failed: /home/dev/project/.dot-agent-deck is mode 0775, which \
                        grants write to group or other — another local account could replace the \
                        coordinator context's directory entry after it is published. The deck \
                        tried to clear those bits and could not, so publishing is refused. On the \
                        machine running the deck, run: chmod go-w \
                        '/home/dev/project/.dot-agent-deck'";
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );
        daemon
            .prepare_results
            .lock()
            .unwrap()
            .push_back(Err(sentence.to_string()));

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

        assert_eq!(error, sentence, "the sentence must arrive verbatim");
        assert!(error.contains("/home/dev/project/.dot-agent-deck"));
        assert!(error.contains("chmod go-w"));
        assert!(error.contains("0775"));
        assert!(daemon.started.lock().unwrap().is_empty());
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

    /// Scenario: the New agent dialog launches a prepared orchestration whose
    /// start role is not Pi (PRD #1223 M6). Every role is started in the
    /// preparation's order through the configured-command start — no command,
    /// agent type or seed of the client's own — with the prepared token, one
    /// shared orchestration id, the run's title and a minted pane id each; then
    /// the coordinator prompt the DECK composed is delivered to the start
    /// role's pane through the acknowledged Runs delivery.
    #[tokio::test]
    async fn a_configured_launch_starts_every_role_and_delivers_to_a_non_pi_coordinator() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("session-planner")),
            [Ok(SendResult::Applied)],
            Ok(SendResult::Applied),
        );
        *daemon.launched_type.lock().unwrap() = Some(AgentType::ClaudeCode);
        let prepared = prepared_workflow();

        let launched = launch_configured_orchestration(
            &daemon,
            "loop",
            Some("demo-orchestrator-1"),
            &prepared,
            32,
            120,
            "orchestration-m6",
        )
        .await
        .unwrap();

        assert_eq!(launched.start_agent_id, "agent-0");
        assert_eq!(launched.agent_ids, ["agent-0", "agent-1"]);
        assert_eq!(
            *daemon.spawn_log.lock().unwrap(),
            ["start:planner", "start:builder"],
            "the preparation's role order"
        );
        assert!(
            daemon
                .start_tokens
                .lock()
                .unwrap()
                .iter()
                .all(|token| token.as_deref() == Some("prep-token-1"))
        );
        let started = daemon.started.lock().unwrap();
        let mut pane_ids = HashSet::new();
        for (index, (options, role)) in started.iter().zip(&prepared.roles).enumerate() {
            assert_eq!(
                options.command, None,
                "the deck runs the configured command"
            );
            assert_eq!(options.agent_type, None);
            assert_eq!(options.seed, None);
            assert_eq!(options.cwd.as_deref(), Some("/canonical/project"));
            assert_eq!(options.display_name.as_deref(), Some(role.name.as_str()));
            let pane_id = options
                .env
                .iter()
                .find(|(key, _)| key == DOT_AGENT_DECK_PANE_ID)
                .map(|(_, value)| value.clone())
                .expect("every role carries a minted pane id");
            assert!(pane_ids.insert(pane_id), "one pane id per role");
            match options.tab_membership.as_ref() {
                Some(TabMembership::Orchestration {
                    name,
                    role_index,
                    role_name,
                    is_start_role,
                    orchestration_cwd,
                    display_title,
                    orchestration_id,
                }) => {
                    assert_eq!(name, "loop");
                    assert_eq!(*role_index, index);
                    assert_eq!(role_name, &role.name);
                    assert_eq!(*is_start_role, role.start);
                    assert_eq!(orchestration_cwd.as_deref(), Some("/canonical/project"));
                    assert_eq!(display_title.as_deref(), Some("demo-orchestrator-1"));
                    assert_eq!(orchestration_id.as_deref(), Some("orchestration-m6"));
                }
                other => panic!("an orchestration role's membership, got {other:?}"),
            }
        }
        drop(started);

        assert_eq!(*daemon.launched_type_queries.lock().unwrap(), ["agent-0"]);
        let submissions = daemon.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].prompt, prepared.prompt);
        assert_eq!(submissions[0].expected_agent_id, "agent-0");
        assert_eq!(
            submissions[0].expected_session_id.as_deref(),
            Some("session-planner")
        );
        drop(submissions);
        assert!(daemon.stopped.lock().unwrap().is_empty());
    }

    /// Scenario: the same launch, but the deck reports the start role it just
    /// started as Pi — so the deck seeded it natively, as the TUI's Pi
    /// coordinators are (PRD #201). The client delivers nothing itself, which
    /// would type the prompt in a second time, and the launch succeeds; the
    /// Runs screen's refusal of a Pi coordinator is not inherited.
    #[tokio::test]
    async fn a_configured_launch_leaves_a_pi_coordinators_prompt_to_the_deck() {
        let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
        *daemon.launched_type.lock().unwrap() = Some(AgentType::Pi);

        let launched = launch_configured_orchestration(
            &daemon,
            "loop",
            None,
            &prepared_workflow(),
            32,
            120,
            "orchestration-pi",
        )
        .await
        .expect("a Pi coordinator launches from this flow");

        assert_eq!(launched.start_agent_id, "agent-0");
        assert!(
            daemon.submissions.lock().unwrap().is_empty(),
            "the deck seeded the Pi coordinator; the client must not deliver a second copy"
        );
        assert!(daemon.stopped.lock().unwrap().is_empty());
        let started = daemon.started.lock().unwrap();
        assert!(
            started.iter().all(|options| matches!(
                options.tab_membership.as_ref(),
                Some(TabMembership::Orchestration {
                    display_title: None,
                    ..
                })
            )),
            "an empty Name sends no title, so the tab falls back to the orchestration's name"
        );
    }

    /// Scenario: the second role is refused mid-launch. The role already
    /// started is stopped again, no coordinator prompt is delivered, and the
    /// error names both the role that failed and the roles that had started —
    /// which the dialog shows inline.
    #[tokio::test]
    async fn a_configured_launch_stops_the_started_roles_when_a_later_role_is_refused() {
        let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
        daemon
            .start_results
            .lock()
            .unwrap()
            .extend([Ok("agent-0".to_string()), refused("builder refused")]);

        let failure = launch_configured_orchestration(
            &daemon,
            "loop",
            Some("run"),
            &prepared_workflow(),
            32,
            120,
            "orchestration-partial",
        )
        .await
        .expect_err("a refused role fails the launch");
        let error = &failure.message;

        assert!(
            error.contains("failed to start orchestration role builder: builder refused"),
            "{error}"
        );
        assert!(error.contains("roles already started: planner"), "{error}");
        assert!(
            error.contains("stopped 1 already-started role(s)"),
            "{error}"
        );
        assert_eq!(*daemon.stopped.lock().unwrap(), ["agent-0"]);
        assert!(daemon.submissions.lock().unwrap().is_empty());
        assert!(daemon.launched_type_queries.lock().unwrap().is_empty());
        assert!(
            failure.unconfirmed_stops.is_empty(),
            "a confirmed rollback carries no cleanup warning"
        );
    }

    /// Scenario: the deck does not advertise `prepared-role-command`, so the
    /// client library withholds the very first role start. Nothing was
    /// started, nothing is stopped, and the refusal says why.
    #[tokio::test]
    async fn a_configured_launch_on_a_deck_without_the_capability_starts_nothing() {
        let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
        daemon.configured_unsupported.store(true, Ordering::SeqCst);

        let failure = launch_configured_orchestration(
            &daemon,
            "loop",
            Some("run"),
            &prepared_workflow(),
            32,
            120,
            "orchestration-older",
        )
        .await
        .expect_err("an older deck cannot launch");
        let error = &failure.message;

        assert!(
            error.contains(CONFIGURED_ROLE_COMMAND_UNSUPPORTED),
            "{error}"
        );
        assert!(error.contains("no role had started"), "{error}");
        assert!(daemon.started.lock().unwrap().is_empty());
        assert!(daemon.stopped.lock().unwrap().is_empty());
        assert!(daemon.submissions.lock().unwrap().is_empty());
    }

    /// A preparation with `names.len()` roles, the first the start role.
    fn prepared_with_roles(names: &[&str]) -> PreparedWorkflow {
        PreparedWorkflow {
            roles: names
                .iter()
                .enumerate()
                .map(|(index, name)| config_role(name, index == 0))
                .collect(),
            ..prepared_workflow()
        }
    }

    /// Scenario (PRD #1223 audit F4): the deck spawns the second role and then
    /// never answers its start. The launch stops waiting at the start bound —
    /// on tokio's paused clock, so the fifteen seconds cost none of the test's
    /// wall clock — reconciles the pane, finds the role that landed, and stops
    /// it along with the one already running.
    #[tokio::test(start_paused = true)]
    async fn a_configured_launch_bounds_a_start_the_deck_never_answers_and_stops_what_landed() {
        let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
        daemon.start_hangs.lock().unwrap().insert(1);
        daemon
            .reconciliation_results
            .lock()
            .unwrap()
            .push_back(Ok(Some("agent-1".to_string())));
        let began = tokio::time::Instant::now();

        let failure = launch_configured_orchestration(
            &daemon,
            "loop",
            Some("run"),
            &prepared_workflow(),
            32,
            120,
            "orchestration-wedged-start",
        )
        .await
        .expect_err("a start the deck never answers fails the launch");
        let error = &failure.message;

        assert_eq!(
            began.elapsed(),
            WORKFLOW_ROLE_START_TIMEOUT,
            "the start bound is what ended the wait"
        );
        assert!(
            error.contains("failed to start orchestration role builder: the deck did not answer the start within 15s"),
            "{error}"
        );
        assert!(error.contains("roles already started: planner"), "{error}");
        assert!(
            error.contains("stopped 2 already-started role(s)"),
            "{error}"
        );
        assert!(!error.contains("cleanup uncertainty"), "{error}");
        assert!(failure.unconfirmed_stops.is_empty());
        assert_eq!(
            *daemon.stopped.lock().unwrap(),
            ["agent-1", "agent-0"],
            "the role that landed without a reply is stopped too, newest first"
        );
        assert_eq!(daemon.reconciliation_requests.lock().unwrap().len(), 1);
        assert!(daemon.submissions.lock().unwrap().is_empty());
    }

    /// Scenario (PRD #1223 audit F4): the same wedged start, but the deck does
    /// not list the role afterwards. It may still spawn it once whatever held
    /// the reply clears, so the launch says it cannot vouch for that role
    /// rather than reporting a clean rollback.
    #[tokio::test(start_paused = true)]
    async fn a_configured_launch_reports_a_timed_out_start_it_cannot_find() {
        let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
        daemon.start_hangs.lock().unwrap().insert(1);

        let failure = launch_configured_orchestration(
            &daemon,
            "loop",
            Some("run"),
            &prepared_workflow(),
            32,
            120,
            "orchestration-unlisted",
        )
        .await
        .expect_err("a start the deck never answers fails the launch");
        let error = &failure.message;

        assert!(
            error.contains("stopped 1 already-started role(s)"),
            "{error}"
        );
        assert!(
            error.contains("cleanup uncertainty: the role's start was not answered"),
            "{error}"
        );
        assert!(
            error.contains("if it starts late it will not be stopped"),
            "{error}"
        );
        assert_eq!(*daemon.stopped.lock().unwrap(), ["agent-0"]);
        assert_eq!(
            failure.unconfirmed_stops,
            ["builder"],
            "the role whose start outcome is unknown is carried as data (audit F6)"
        );
    }

    /// Scenario (PRD #1223 audit F4): the fourth role is refused, and of the
    /// three rollback stops the newest is never answered, the middle one is
    /// confirmed and the oldest is refused. Each stop is bounded on its own,
    /// so the rollback reaches all three instead of waiting behind the first,
    /// and the error names EVERY role whose stop it could not confirm.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_bounds_each_stop_and_names_every_role_it_could_not_confirm() {
        let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
        daemon.start_results.lock().unwrap().extend([
            Ok("agent-0".to_string()),
            Ok("agent-1".to_string()),
            Ok("agent-2".to_string()),
            refused("tester refused"),
        ]);
        daemon
            .stop_hangs
            .lock()
            .unwrap()
            .insert("agent-2".to_string());
        daemon
            .stop_errors
            .lock()
            .unwrap()
            .insert("agent-0".to_string(), "stop refused".to_string());
        let began = tokio::time::Instant::now();

        let failure = launch_configured_orchestration(
            &daemon,
            "loop",
            Some("run"),
            &prepared_with_roles(&["planner", "builder", "reviewer", "tester"]),
            32,
            120,
            "orchestration-wedged-stop",
        )
        .await
        .expect_err("a refused role fails the launch");
        let error = &failure.message;

        assert_eq!(
            *daemon.stop_attempts.lock().unwrap(),
            ["agent-2", "agent-1", "agent-0"],
            "every started role is asked to stop, newest first, past the wedged one"
        );
        assert_eq!(*daemon.stopped.lock().unwrap(), ["agent-1"]);
        assert_eq!(
            began.elapsed(),
            WORKFLOW_ROLE_STOP_TIMEOUT,
            "one wedged stop costs one stop bound, not the rollback"
        );
        assert!(
            error.contains("cleanup could not confirm stop for 2 of 3 already-started role(s)"),
            "{error}"
        );
        assert!(
            error.contains("reviewer (agent-2: the deck did not answer the stop within 15s)"),
            "{error}"
        );
        assert!(error.contains("planner (agent-0: stop refused)"), "{error}");
        assert_eq!(
            failure.unconfirmed_stops,
            ["reviewer", "planner"],
            "every role whose stop was not confirmed is carried as data, newest first (audit F6)"
        );
    }

    /// Scenario (PRD #1223 U4): an orchestration close over four roles, where
    /// the deck confirms two, refuses one and never answers one. The stops run
    /// CONCURRENTLY — every role is asked, and the whole close costs one stop
    /// bound rather than one per role behind the wedged stop — and the
    /// outcomes line up with the roles: the refused and the unanswered role are
    /// named as unconfirmed with their reasons, and the two confirmed ones are
    /// not.
    #[tokio::test(start_paused = true)]
    async fn closing_an_orchestration_stops_every_role_at_once_and_names_the_unconfirmed() {
        let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
        daemon
            .stop_hangs
            .lock()
            .unwrap()
            .insert("agent-1".to_string());
        daemon
            .stop_errors
            .lock()
            .unwrap()
            .insert("agent-2".to_string(), "stop refused".to_string());
        let roles: Vec<StartedRole> = ["planner", "builder", "reviewer", "tester"]
            .iter()
            .enumerate()
            .map(|(index, role)| StartedRole {
                agent_id: format!("agent-{index}"),
                role: role.to_string(),
            })
            .collect();
        let began = tokio::time::Instant::now();

        let outcomes = stop_roles_concurrently(&daemon, &roles).await;

        let mut attempts = daemon.stop_attempts.lock().unwrap().clone();
        attempts.sort();
        assert_eq!(
            attempts,
            ["agent-0", "agent-1", "agent-2", "agent-3"],
            "every role is asked"
        );
        let mut stopped = daemon.stopped.lock().unwrap().clone();
        stopped.sort();
        assert_eq!(stopped, ["agent-0", "agent-3"]);
        assert_eq!(
            began.elapsed(),
            WORKFLOW_ROLE_STOP_TIMEOUT,
            "concurrent: one wedged stop costs one bound for the whole close"
        );
        assert_eq!(outcomes.len(), 4, "one outcome per role, aligned");
        assert!(outcomes[0].is_none());
        assert_eq!(
            outcomes[1],
            Some(UnconfirmedStop {
                role: "builder".into(),
                reason: "agent-1: the deck did not answer the stop within 15s".into(),
            })
        );
        assert_eq!(
            outcomes[2],
            Some(UnconfirmedStop {
                role: "reviewer".into(),
                reason: "agent-2: stop refused".into(),
            })
        );
        assert!(outcomes[3].is_none());
    }

    /// Scenario (PRD #1223 U4): the webview sends a `stop_agent` or a
    /// `stop_orchestration` that names no deck. Both fail to decode, for
    /// `start_agent`'s reason — a stop must never fall back to the selection.
    #[test]
    fn a_stop_without_a_deck_does_not_decode() {
        for action in [
            serde_json::json!({"type": "stop_agent", "agentId": "7"}),
            serde_json::json!({"type": "stop_orchestration", "roles": [{"agentId": "7", "name": "planner"}]}),
        ] {
            assert!(
                serde_json::from_value::<DesktopAction>(action.clone()).is_err(),
                "{action} must not decode"
            );
        }
        assert!(matches!(
            serde_json::from_value::<DesktopAction>(serde_json::json!({"type": "stop_agent", "deckId": "deck-000000000000dec1", "agentId": "7"})),
            Ok(DesktopAction::StopAgent { deck_id, agent_id }) if deck_id == "deck-000000000000dec1" && agent_id == "7"
        ));
    }

    /// Scenario (PRD #1223 audit F4): the Runs launch shares the rollback, and
    /// its starts are bounded the same way — a second role the deck never
    /// answers ends the launch at the start bound and the first is stopped.
    #[tokio::test(start_paused = true)]
    async fn a_runs_launch_bounds_a_start_the_deck_never_answers() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );
        daemon.start_hangs.lock().unwrap().insert(1);

        let failure = launch_workflow(
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
        let error = &failure.message;

        assert!(
            error.contains("failed to start workflow role builder: the deck did not answer the start within 15s"),
            "{error}"
        );
        assert!(
            error.contains("stopped 1 already-started role(s)"),
            "{error}"
        );
        assert!(
            error.contains("if it starts late it will not be stopped"),
            "{error}"
        );
        assert_eq!(*daemon.stopped.lock().unwrap(), ["agent-0"]);
        assert_eq!(
            failure.unconfirmed_stops,
            ["builder"],
            "the Runs launch carries the role it could not vouch for as data (audit V2)"
        );
    }

    /// Scenario (PRD #1223 audit V5): a real `DaemonClient` against a scripted
    /// deck that takes the start's connection, READS the request and then drops
    /// it without answering — the lost reply of a request the deck may well
    /// have acted on. The failure must classify as indeterminate, so the
    /// reconciliation that follows reports a role it cannot find as cleanup it
    /// could not confirm. The same deck's `ok: false` refusal of the next start
    /// must classify as definitive: the deck answered, so nothing is pending.
    ///
    /// Only the classification is exercised here; what the launch then does
    /// with it is `an_indeterminate_start_the_deck_does_not_list_is_reported_as_unconfirmed`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_start_whose_connection_drops_is_indeterminate_and_a_refusal_is_not() {
        use dot_agent_deck::daemon_protocol::{
            AttachResponse, DAEMON_CAPABILITIES, KIND_REQ, KIND_RESP, PROTOCOL_VERSION, read_frame,
            write_frame,
        };
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("a scratch dir for the socket");
        let socket = dir.path().join("s");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted deck");
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");
        // Each call opens its own short-lived connection, and
        // `start_prepared_role` re-handshakes before every start — so the
        // script is: hello, the start that is dropped, hello, the start that is
        // refused.
        let deck = tokio::spawn(async move {
            let mut answered_starts = 0usize;
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    return;
                };
                let (mut reader, mut writer) = stream.into_split();
                let Ok(Some((KIND_REQ, payload))) = read_frame(&mut reader).await else {
                    continue;
                };
                let request: serde_json::Value =
                    serde_json::from_slice(&payload).unwrap_or_default();
                if request["op"] == "hello" {
                    let mut reply = AttachResponse::hello(PROTOCOL_VERSION);
                    reply.capabilities = Some(
                        DAEMON_CAPABILITIES
                            .iter()
                            .map(|capability| (*capability).to_string())
                            .collect(),
                    );
                    let encoded = serde_json::to_vec(&reply).expect("serialize the hello");
                    let _ = write_frame(&mut writer, KIND_RESP, &encoded).await;
                    continue;
                }
                answered_starts += 1;
                if answered_starts == 1 {
                    // The request was read and the connection goes away with no
                    // reply — the deck may have started the role.
                    drop(writer);
                    continue;
                }
                let encoded = serde_json::to_vec(&AttachResponse::err("builder refused"))
                    .expect("serialize the refusal");
                let _ = write_frame(&mut writer, KIND_RESP, &encoded).await;
            }
        });

        let client = DaemonClient::new(socket);
        let options = || StartAgentOptions {
            command: None,
            cwd: Some("/canonical/project".into()),
            display_name: Some("builder".into()),
            rows: 24,
            cols: 80,
            env: vec![(DOT_AGENT_DECK_PANE_ID.into(), mint_desktop_pane_id())],
            tab_membership: None,
            agent_type: None,
            seed: None,
        };

        let dropped = WorkflowDaemon::start_configured_role(&client, options(), "prep-token-1")
            .await
            .expect_err("a dropped connection is a failed start");
        let answered = WorkflowDaemon::start_configured_role(&client, options(), "prep-token-1")
            .await
            .expect_err("the deck refused this one");
        deck.abort();

        assert!(
            dropped.indeterminate,
            "a start whose reply was lost may still have been acted on: {}",
            dropped.message
        );
        assert!(
            dropped
                .message
                .contains("closed connection before sending RESP"),
            "the dropped connection is what failed, not something earlier: {}",
            dropped.message
        );
        assert!(
            !answered.indeterminate,
            "a refusal the deck composed means it started nothing: {}",
            answered.message
        );
        assert!(
            answered.message.contains("builder refused"),
            "{}",
            answered.message
        );
    }

    /// Scenario (PRD #1223 audit V5): a configured role's start fails with its
    /// reply LOST rather than refused — the request may have reached the deck —
    /// and the reconciliation that follows lists nothing. The launch cannot say
    /// that role did not start, so it reports it as cleanup it could not
    /// confirm, exactly as it does for a start that timed out. A start the deck
    /// ANSWERED with a refusal in the same shape reports no such uncertainty,
    /// which is the half that keeps the warning meaningful.
    #[tokio::test]
    async fn an_indeterminate_start_the_deck_does_not_list_is_reported_as_unconfirmed() {
        for (script, expected_uncertainty) in [
            (lost("the connection closed before the reply"), true),
            (refused("builder refused"), false),
        ] {
            let daemon = FakeWorkflowDaemon::new(Ok(None), [], Ok(SendResult::Applied));
            daemon
                .start_results
                .lock()
                .unwrap()
                .extend([Ok("agent-0".to_string()), script]);

            let failure = launch_configured_orchestration(
                &daemon,
                "loop",
                Some("run"),
                &prepared_workflow(),
                32,
                120,
                "orchestration-indeterminate",
            )
            .await
            .unwrap_err();

            // Either way the deck was asked, and the role it did start was
            // stopped: only the report about the role that failed differs.
            assert_eq!(daemon.reconciliation_requests.lock().unwrap().len(), 1);
            assert_eq!(*daemon.stopped.lock().unwrap(), ["agent-0"]);
            assert_eq!(
                failure.message.contains("cleanup uncertainty"),
                expected_uncertainty,
                "{}",
                failure.message
            );
            assert_eq!(
                failure.unconfirmed_stops,
                if expected_uncertainty {
                    vec!["builder".to_string()]
                } else {
                    Vec::new()
                },
                "{}",
                failure.message
            );
        }
    }

    /// Scenario (PRD #1223 audit V2): the Runs launch's composite failure — the
    /// deck refuses the second role as `stale-preparation` after the first had
    /// started, and the rollback's stop of the first is refused. The sentence
    /// carries the refusal code the Runs screen translates into "Nothing was
    /// started", so the role that may still be running has to arrive as data
    /// for the screen to put the cleanup warning first.
    #[tokio::test]
    async fn a_runs_launch_carries_the_roles_its_rollback_could_not_stop_as_data() {
        let daemon = FakeWorkflowDaemon::new(
            Ok(Some("unused-session")),
            std::iter::empty(),
            Ok(SendResult::Applied),
        );
        daemon.start_results.lock().unwrap().extend([
            Ok("agent-0".to_string()),
            refused(&format!(
                "{}: the coordinator context changed since it was prepared",
                dot_agent_deck::daemon_protocol::PROJECT_ERR_STALE_PREPARATION
            )),
        ]);
        daemon
            .stop_errors
            .lock()
            .unwrap()
            .insert("agent-0".to_string(), "stop refused".to_string());

        let failure = launch_workflow(
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

        assert!(
            failure.message.contains("stale-preparation: "),
            "{}",
            failure.message
        );
        assert!(
            failure
                .message
                .contains("cleanup could not confirm stop for 1 of 1 already-started role(s)"),
            "{}",
            failure.message
        );
        assert_eq!(failure.unconfirmed_stops, ["planner"]);
        // What `desktop_run_action` rejects with for it: the structured shape,
        // not the bare string the Runs screen used to classify by substring.
        let message = failure.message.clone();
        match crate::dto::DesktopActionError::launch(failure.message, failure.unconfirmed_stops) {
            crate::dto::DesktopActionError::LaunchCleanup(cleanup) => {
                assert_eq!(cleanup.message, message);
                assert_eq!(cleanup.unconfirmed_stops, ["planner"]);
            }
            other => panic!(
                "a Runs launch with an unconfirmed stop must reject with the structured shape: {other:?}"
            ),
        }
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

        let failure = launch_workflow(
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
        let error = &failure.message;

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

        let failure = launch_workflow(
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
        let error = &failure.message;

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
        daemon
            .start_results
            .lock()
            .unwrap()
            .extend([Ok("agent-builder".to_string()), lost("start response lost")]);
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

        let failure = launch_workflow(
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
        let error = &failure.message;

        assert!(error.contains("start response lost"));
        assert!(error.contains("stopped 2 already-started role(s)"));
        assert!(!error.contains("cleanup uncertainty"));
        assert!(failure.unconfirmed_stops.is_empty());
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
            .push_back(lost("start response lost"));
        daemon
            .reconciliation_results
            .lock()
            .unwrap()
            .push_back(Err("list-agents unavailable".to_string()));

        let failure = launch_workflow(
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
        let error = &failure.message;

        assert!(error.contains("cleanup uncertainty"));
        assert!(error.contains("list-agents unavailable"));
        assert!(error.contains("stopped 0 already-started role(s)"));
        assert_eq!(failure.unconfirmed_stops, ["planner"]);
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
        let disconnected =
            crate::dto::disconnected_snapshot(&Endpoint::local(), "daemon start timed out");
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

    /// One event frame, split so the daemon can stall in the middle of its
    /// five-byte header (issue #1028).
    ///
    /// The `SessionStart` the coordinator readiness wait is looking for, encoded
    /// as the `KIND_EVENT` frame a real daemon would push.
    #[cfg(unix)]
    fn session_start_frame(pane_id: &str, agent_id: &str, session_id: &str) -> Vec<u8> {
        let payload = serde_json::to_vec(&serde_json::json!({
            "kind": "event",
            "session_id": session_id,
            "agent_type": "claude_code",
            "event_type": "session_start",
            "timestamp": "2026-01-01T00:00:00Z",
            "pane_id": pane_id,
            "agent_id": agent_id,
        }))
        .expect("encode the broadcast");
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(dot_agent_deck::daemon_protocol::KIND_EVENT);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// Scenario: a scripted daemon accepts the coordinator-readiness
    /// subscription and then stalls **two bytes into** an event frame's
    /// five-byte header. The desktop's first readiness wait expires on that
    /// partial header and reports "not ready yet"; the daemon then writes the
    /// rest of the frame, and a second wait on the SAME watch must still deliver
    /// the coordinator's `SessionStart`.
    ///
    /// This is issue #1028's defect as a runtime ordering, not as a shape:
    /// `tokio::time::timeout` drops the future it is holding when it expires, and
    /// `EventSubscription::next_event` is not cancel-safe — the two header bytes
    /// lived in `read_frame`'s local buffer, so the old code lost them and
    /// misparsed every frame that followed. Against that code this test fails on
    /// the second wait.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_readiness_timeout_mid_frame_header_leaves_the_watch_usable() {
        use dot_agent_deck::daemon_protocol::{
            AttachResponse, KIND_REQ, KIND_RESP, read_frame, write_frame,
        };
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::AsyncWriteExt;

        let pane_id = "pane-1028";
        let agent_id = "agent-1028";
        let session_id = "session-1028";
        let frame = session_start_frame(pane_id, agent_id, session_id);

        let dir = tempfile::tempdir().expect("a scratch dir for the socket");
        let socket = dir.path().join("s");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");
        // The client refuses a socket the ambient umask left group- or
        // world-accessible, the same reason `daemon_bridge`'s fixtures restate
        // the mode rather than borrowing `IpcListener::bind`'s umask dance.
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");

        // Raised by the test once its first readiness wait has timed out, so the
        // daemon completes the header at exactly the moment that matters rather
        // than on a sleep that could drift either way.
        let (timed_out_tx, timed_out_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept the subscriber");
            let (mut reader, mut writer) = stream.into_split();
            let (kind, _payload) = read_frame(&mut reader)
                .await
                .expect("read the subscribe-events request")
                .expect("the client sent a frame");
            assert_eq!(kind, KIND_REQ);
            let reply = serde_json::to_vec(&AttachResponse {
                ok: true,
                ..Default::default()
            })
            .expect("encode the subscribe reply");
            write_frame(&mut writer, KIND_RESP, &reply)
                .await
                .expect("accept the subscription");

            // Two of the five header bytes, and then nothing. A reader parked
            // here is parked mid-header, which is the whole condition.
            writer
                .write_all(&frame[..2])
                .await
                .expect("write a partial frame header");
            writer.flush().await.expect("flush the partial header");

            timed_out_rx
                .await
                .expect("the test reports its first timeout");

            writer
                .write_all(&frame[2..])
                .await
                .expect("complete the frame");
            writer.flush().await.expect("flush the completed frame");
            // Hold the connection open: an EOF racing the frame above would let
            // a desynchronised reader end the stream for the wrong reason and
            // pass this test for the wrong reason with it.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let client = DaemonClient::new(socket);
        let mut watch = client
            .begin_coordinator_readiness()
            .await
            .expect("subscribe to the scripted daemon");

        assert_eq!(
            client
                .wait_for_coordinator_readiness(
                    &mut watch,
                    pane_id,
                    agent_id,
                    Duration::from_millis(200)
                )
                .await,
            Ok(None),
            "a readiness wait that expires must report 'not ready yet'"
        );
        let _ = timed_out_tx.send(());

        assert_eq!(
            client
                .wait_for_coordinator_readiness(
                    &mut watch,
                    pane_id,
                    agent_id,
                    Duration::from_secs(5)
                )
                .await,
            Ok(Some(session_id.to_string())),
            "the watch must still be synchronised after a timeout dropped a wait \
             mid-header — issue #1028"
        );

        server.abort();
    }

    /// Scenario: a scripted daemon accepts the coordinator-readiness
    /// subscription, reads the `SubscribeEvents` request and then answers
    /// nothing at all, holding the socket open. `begin_coordinator_readiness`
    /// must come back with an error naming the deck as wedged rather than
    /// parking the launch for as long as the peer keeps the connection.
    ///
    /// Issue #1084, and the same defect PRD #742 M14 fixed one path over: a deck
    /// that is DOWN fails at once with `ECONNREFUSED`, so the failure this
    /// covers is the one where the connect succeeds. **Measured against the
    /// unbounded code**, where it fails on the outer bound below rather than on
    /// either assertion; take that bound away too and it does not fail at all,
    /// it never returns. That is why the outer bound is here, exactly as in
    /// `daemon_bridge`'s sibling test for the handshake.
    ///
    /// # Paused time, and paused at a POINT
    ///
    /// The clock is stopped only once `accepted_rx` resolves, which is the peer
    /// confirming it took the connection and read the request. `start_paused`
    /// would auto-advance from the first moment the runtime had nothing to poll,
    /// so a clock that jumped while the connect was still in flight would report
    /// the same error for a scenario nobody wrote. Pausing here leaves exactly
    /// one thing outstanding — a read against a peer that will never write — and
    /// the runtime advances to the only deadline left. Real socket I/O, real
    /// silence, none of the fifteen seconds spent.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_deck_that_never_answers_the_readiness_subscription_is_reported_rather_than_awaited()
    {
        use dot_agent_deck::daemon_protocol::read_frame;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("a scratch dir for the socket");
        let socket = dir.path().join("s");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the scripted daemon");
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");

        let (accepted, accepted_rx) = tokio::sync::oneshot::channel::<()>();
        let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
        let silent = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.expect("accept the subscriber");
            // The write half is kept bound rather than dropped: dropping it
            // shuts the socket down and the client would read EOF instead of
            // the stall this test is about.
            let (mut reader, _writer) = stream.into_split();
            let _ = read_frame(&mut reader).await;
            let _ = accepted.send(());
            let _ = release_rx.await;
        });

        let subscribing = {
            let socket = socket.clone();
            tokio::spawn(async move {
                DaemonClient::new(socket)
                    .begin_coordinator_readiness()
                    .await
            })
        };
        accepted_rx
            .await
            .expect("the deck must have taken the connection and read the request");
        tokio::time::pause();

        // Under a paused clock tokio advances to the NEAREST deadline, so the
        // bound under test fires first while it has one, and this outer bound
        // fires only when it does not. Neither costs wall clock; without it a
        // regression is a hung job rather than a red test.
        let outcome = tokio::time::timeout(Duration::from_secs(600), subscribing)
            .await
            .expect(
                "begin_coordinator_readiness() must bound its own wait rather than await a \
                 reply that is not coming",
            )
            .expect("the subscribing task must not panic");

        let _ = release.send(());
        let _ = silent.await;

        let error = outcome.expect_err("a deck that never answers must not resolve as subscribed");
        assert!(
            error.contains("did not answer"),
            "the elapsed case must say the deck took the connection and stalled, rather than \
             reading as a transport failure: {error}"
        );
        assert!(
            error.contains(
                &crate::daemon_bridge::DECK_REPLY_TIMEOUT
                    .as_secs()
                    .to_string()
            ),
            "and name the bound it exceeded: {error}"
        );
    }
}
