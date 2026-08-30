use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dot_agent_deck::agent_pty::{
    AgentRecord, PTY_RESIZE_DIM_MAX, TabMembership, is_valid_cwd, is_valid_display_name,
    is_valid_orchestration_cwd, is_valid_pane_id_env,
};
use dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;
use dot_agent_deck::event::{AgentType, SendResult};
use dot_agent_deck::state::SessionStatus;
use serde::{Deserialize, Serialize};
use tauri::ipc::JavaScriptChannelId;

pub(crate) const TERMINAL_INPUT_MAX_BYTES: usize = 64 * 1024;
pub(crate) const COMMAND_MAX_BYTES: usize = 64 * 1024;
const AGENT_ID_MAX_BYTES: usize = 256;
const ERROR_MESSAGE_MAX_CHARS: usize = 2048;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSnapshot {
    pub connection: DesktopConnection,
    pub agents: Vec<DesktopAgent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_cwd: Option<String>,
    pub protocol_version: u32,
    pub source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopConnection {
    pub status: ConnectionStatus,
    pub socket_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub client_protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_protocol_version: Option<u32>,
    pub client_build_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_build_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_agent_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
    Incompatible,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAgent {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub agent_type: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tool: Option<DesktopActiveTool>,
    pub tool_count: u32,
    pub tab: DesktopTab,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActiveTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopTab {
    Dashboard,
    Mode {
        name: String,
    },
    Orchestration {
        name: String,
        role_index: usize,
        role_name: String,
        is_start_role: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        orchestration_id: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BootstrapOptions {
    pub start_if_missing: bool,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopAction {
    Refresh,
    Bootstrap {
        #[serde(default)]
        start_if_missing: bool,
    },
    StartAgent {
        command: Option<String>,
        cwd: Option<String>,
        display_name: Option<String>,
        rows: Option<u16>,
        cols: Option<u16>,
    },
    StartWorkflow {
        name: String,
        cwd: String,
        task_prompt: String,
        roles: Vec<WorkflowRoleInput>,
        rows: Option<u16>,
        cols: Option<u16>,
    },
    StopAgent {
        agent_id: String,
    },
    StopDaemon {
        #[serde(default)]
        force: bool,
    },
    RestartDaemon,
    RenameAgent {
        agent_id: String,
        #[serde(alias = "name")]
        display_name: String,
    },
    AttachTerminal {
        agent_id: String,
        on_output: JavaScriptChannelId,
    },
    DetachTerminal {
        session_id: String,
    },
    SubmitText {
        agent_id: String,
        text: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRoleInput {
    pub role: String,
    pub command: String,
    #[serde(default)]
    pub start: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActionResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_result: Option<SendResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalAttachResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub snapshot: DesktopSnapshot,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalAttachResult {
    pub session_id: String,
    pub agent_id: String,
    pub generation: u64,
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStateEvent {
    pub session_id: String,
    pub agent_id: String,
    pub generation: u64,
    pub state: TerminalState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Attached,
    End,
    Error,
}

fn agent_type_name(agent_type: &AgentType) -> &'static str {
    match agent_type {
        AgentType::ClaudeCode => "claude_code",
        AgentType::OpenCode => "open_code",
        AgentType::Pi => "pi",
        AgentType::Codex => "codex",
        AgentType::Devin => "devin",
        AgentType::None => "none",
    }
}

fn session_status_name(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "thinking",
        SessionStatus::Working => "working",
        SessionStatus::Compacting => "compacting",
        SessionStatus::WaitingForInput => "waiting_for_input",
        SessionStatus::Idle => "idle",
        SessionStatus::Error => "error",
        SessionStatus::Unknown => "unknown",
    }
}

fn map_tab(tab: Option<&TabMembership>) -> DesktopTab {
    match tab {
        None => DesktopTab::Dashboard,
        Some(TabMembership::Mode { name }) => DesktopTab::Mode { name: name.clone() },
        Some(TabMembership::Orchestration {
            name,
            role_index,
            role_name,
            is_start_role,
            display_title,
            orchestration_id,
            ..
        }) => DesktopTab::Orchestration {
            name: name.clone(),
            role_index: *role_index,
            role_name: role_name.clone(),
            is_start_role: *is_start_role,
            display_title: display_title.clone(),
            orchestration_id: orchestration_id.clone(),
        },
    }
}

pub(crate) fn map_agent(record: AgentRecord) -> DesktopAgent {
    let live = record.live.as_ref();
    let agent_type = live
        .and_then(|snapshot| snapshot.agent_type.as_ref())
        .or(record.agent_type.as_ref())
        .map(agent_type_name)
        .unwrap_or("none")
        .to_string();
    let status = live
        .map(|snapshot| session_status_name(&snapshot.status))
        .unwrap_or("running")
        .to_string();
    let active_tool = live
        .and_then(|snapshot| snapshot.active_tool.as_ref())
        .map(|tool| DesktopActiveTool {
            name: tool.name.clone(),
            detail: tool.detail.clone(),
        });
    let tool_count = live.map(|snapshot| snapshot.tool_count).unwrap_or(0);
    let tab = map_tab(record.tab_membership.as_ref());

    DesktopAgent {
        id: record.id,
        pane_id: record.pane_id_env,
        display_name: record.display_name,
        cwd: record.cwd,
        rows: record.rows,
        cols: record.cols,
        agent_type,
        status,
        active_tool,
        tool_count,
        tab,
    }
}

pub(crate) fn safe_message(message: impl AsRef<str>) -> String {
    message
        .as_ref()
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(ERROR_MESSAGE_MAX_CHARS)
        .collect()
}

pub(crate) fn socket_path_text() -> String {
    safe_message(
        dot_agent_deck::config::attach_socket_path()
            .to_string_lossy()
            .as_ref(),
    )
}

pub(crate) fn desktop_project_cwd() -> Option<String> {
    let project_dir = option_env!("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .and_then(|path| {
            path.parent()
                .and_then(|path| path.parent())
                .map(std::path::Path::to_path_buf)
        })
        .filter(|path| path.join(".dot-agent-deck.toml").is_file())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .filter(|path| path.join(".dot-agent-deck.toml").is_file())
        })?;
    Some(safe_message(project_dir.to_string_lossy()))
}

pub(crate) fn disconnected_snapshot(error: impl AsRef<str>) -> DesktopSnapshot {
    DesktopSnapshot {
        connection: DesktopConnection {
            status: ConnectionStatus::Disconnected,
            socket_path: socket_path_text(),
            error: Some(safe_message(error)),
            client_protocol_version: PROTOCOL_VERSION,
            server_protocol_version: None,
            client_build_version: dot_agent_deck::build_id::local_build_id(),
            daemon_build_version: None,
            daemon_version: None,
            running_agent_count: None,
        },
        agents: Vec::new(),
        project_cwd: desktop_project_cwd(),
        protocol_version: PROTOCOL_VERSION,
        source: "daemon",
    }
}

pub(crate) fn validate_agent_id(agent_id: &str) -> Result<(), String> {
    if agent_id.is_empty()
        || agent_id.len() > AGENT_ID_MAX_BYTES
        || agent_id.chars().any(char::is_control)
    {
        return Err(format!(
            "agentId must be 1..={AGENT_ID_MAX_BYTES} bytes without control characters"
        ));
    }
    Ok(())
}

pub(crate) fn validate_dimensions(rows: u16, cols: u16) -> Result<(u16, u16), String> {
    if rows == 0 || cols == 0 {
        return Err(format!(
            "rows and cols must be greater than zero (got {rows}x{cols})"
        ));
    }
    Ok((rows.min(PTY_RESIZE_DIM_MAX), cols.min(PTY_RESIZE_DIM_MAX)))
}

pub(crate) fn validate_command(command: Option<&str>) -> Result<(), String> {
    if let Some(command) = command {
        if command.trim().is_empty() {
            return Err("command must not be empty or whitespace-only".into());
        }
        if command.len() > COMMAND_MAX_BYTES || command.contains('\0') {
            return Err(format!(
                "command must be at most {COMMAND_MAX_BYTES} bytes and contain no NUL"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_start_fields(
    command: Option<&str>,
    cwd: Option<&str>,
    display_name: Option<&str>,
    rows: u16,
    cols: u16,
) -> Result<(u16, u16), String> {
    validate_command(command)?;
    if let Some(cwd) = cwd
        && !is_valid_cwd(cwd)
    {
        return Err("cwd is invalid, oversized, empty, or contains control characters".into());
    }
    if let Some(display_name) = display_name
        && !is_valid_display_name(display_name)
    {
        return Err(
            "displayName is invalid, oversized, empty, or contains control characters".into(),
        );
    }
    validate_dimensions(rows, cols)
}

pub(crate) fn validate_terminal_input(data: &[u8]) -> Result<(), String> {
    if data.len() > TERMINAL_INPUT_MAX_BYTES {
        return Err(format!(
            "terminal input chunk exceeds the {TERMINAL_INPUT_MAX_BYTES}-byte limit"
        ));
    }
    Ok(())
}

pub(crate) fn mint_desktop_pane_id() -> String {
    static NONCE: OnceLock<u64> = OnceLock::new();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nonce = *NONCE.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        if let Ok(duration) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            duration.as_nanos().hash(&mut hasher);
        }
        hasher.finish()
    });
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pane_id = format!("desktop-{nonce:016x}-{sequence}");
    debug_assert!(is_valid_pane_id_env(&pane_id));
    pane_id
}

pub(crate) fn validate_workflow_shape(
    name: &str,
    cwd: &str,
    roles: &[WorkflowRoleInput],
    rows: u16,
    cols: u16,
) -> Result<(u16, u16), String> {
    const MAX_WORKFLOW_ROLES: usize = 16;
    if !is_valid_display_name(name) {
        return Err(
            "workflow name is invalid, oversized, empty, or contains control characters".into(),
        );
    }
    if !is_valid_orchestration_cwd(cwd) {
        return Err("workflow cwd must be a valid absolute path without control characters".into());
    }
    if roles.is_empty() || roles.len() > MAX_WORKFLOW_ROLES {
        return Err(format!(
            "workflow roles must contain 1..={MAX_WORKFLOW_ROLES} entries"
        ));
    }
    let mut names = HashSet::with_capacity(roles.len());
    let mut start_count = 0usize;
    for role in roles {
        if !is_valid_display_name(&role.role) {
            return Err(format!(
                "invalid workflow role name: {}",
                safe_message(&role.role)
            ));
        }
        if !names.insert(role.role.as_str()) {
            return Err(format!(
                "duplicate workflow role: {}",
                safe_message(&role.role)
            ));
        }
        validate_command(Some(&role.command))?;
        start_count += usize::from(role.start);
    }
    if start_count != 1 {
        return Err(format!(
            "workflow must define exactly one start role (found {start_count})"
        ));
    }
    validate_dimensions(rows, cols)
}

pub(crate) fn ensure_desktop_workflow_platform_supported(target_os: &str) -> Result<(), String> {
    if target_os == "windows" {
        return Err(
            "desktop workflow launch is unavailable on Windows in this preview because profile commands are POSIX-shell quoted; use the TUI or launch commands manually until native Windows command construction is implemented"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::state::{ActiveTool, SessionSnapshot};

    fn fixture_record() -> AgentRecord {
        AgentRecord {
            id: "agent-7".into(),
            pane_id_env: Some("pane-7".into()),
            display_name: Some("builder".into()),
            cwd: Some("/tmp/project".into()),
            tab_membership: Some(TabMembership::Mode {
                name: "build".into(),
            }),
            agent_type: Some(AgentType::Codex),
            rows: 32,
            cols: 120,
            live: Some(SessionSnapshot {
                status: SessionStatus::Working,
                agent_type: Some(AgentType::Codex),
                active_tool: Some(ActiveTool {
                    name: "shell".into(),
                    detail: Some("cargo test".into()),
                }),
                tool_count: 4,
                first_prompts: Vec::new(),
                last_user_prompt: None,
                live_target: None,
            }),
        }
    }

    #[test]
    fn agent_mapping_is_frontend_stable() {
        let value = serde_json::to_value(map_agent(fixture_record())).unwrap();
        assert_eq!(value["id"], "agent-7");
        assert_eq!(value["paneId"], "pane-7");
        assert_eq!(value["agentType"], "codex");
        assert_eq!(value["status"], "working");
        assert_eq!(value["activeTool"]["name"], "shell");
        assert_eq!(value["tab"]["kind"], "mode");
    }

    #[test]
    fn record_without_hook_state_is_still_running() {
        let mut record = fixture_record();
        record.live = None;
        let mapped = map_agent(record);
        assert_eq!(mapped.status, "running");
        assert_eq!(mapped.tool_count, 0);
        assert!(mapped.active_tool.is_none());
    }

    #[test]
    fn asymmetric_dimensions_keep_rows_then_cols_and_clamp() {
        assert_eq!(validate_dimensions(50, 200).unwrap(), (50, 200));
        assert!(validate_dimensions(0, 200).is_err());
        assert_eq!(
            validate_dimensions(u16::MAX, u16::MAX).unwrap(),
            (PTY_RESIZE_DIM_MAX, PTY_RESIZE_DIM_MAX)
        );
    }

    #[test]
    fn terminal_input_is_bounded() {
        assert!(validate_terminal_input(&vec![0; TERMINAL_INPUT_MAX_BYTES]).is_ok());
        assert!(validate_terminal_input(&vec![0; TERMINAL_INPUT_MAX_BYTES + 1]).is_err());
    }

    #[test]
    fn command_validation_rejects_blank_nul_and_oversize_values() {
        assert!(validate_command(None).is_ok());
        assert!(validate_command(Some("codex --model gpt-5.6-sol")).is_ok());
        assert!(validate_command(Some("  ")).is_err());
        assert!(validate_command(Some("codex\0oops")).is_err());
        let oversized = "x".repeat(COMMAND_MAX_BYTES + 1);
        assert!(validate_command(Some(&oversized)).is_err());
    }

    #[test]
    fn profile_start_action_uses_camel_case_fields() {
        let action: DesktopAction = serde_json::from_value(serde_json::json!({
            "type": "start_agent",
            "command": "codex --model gpt-5.6-sol",
            "cwd": "/tmp/project",
            "displayName": "builder",
            "rows": 30,
            "cols": 110
        }))
        .unwrap();
        assert!(matches!(
            action,
            DesktopAction::StartAgent {
                display_name: Some(name),
                rows: Some(30),
                cols: Some(110),
                ..
            } if name == "builder"
        ));
    }

    #[test]
    fn restart_daemon_action_has_no_force_field() {
        let action: DesktopAction = serde_json::from_value(serde_json::json!({
            "type": "restart_daemon"
        }))
        .unwrap();
        assert!(matches!(action, DesktopAction::RestartDaemon));
    }

    #[test]
    fn disconnected_snapshot_is_fixture_safe_and_sanitized() {
        let snapshot = disconnected_snapshot("offline\u{1b}[31m");
        assert_eq!(snapshot.connection.status, ConnectionStatus::Disconnected);
        assert_eq!(snapshot.connection.error.as_deref(), Some("offline[31m"));
        assert!(snapshot.agents.is_empty());
        let value = serde_json::to_value(snapshot).unwrap();
        assert_eq!(value["connection"]["status"], "disconnected");
        assert_eq!(value["source"], "daemon");
    }

    #[test]
    fn desktop_pane_ids_are_unique_and_pass_daemon_validation() {
        let first = mint_desktop_pane_id();
        let second = mint_desktop_pane_id();
        assert_ne!(first, second);
        assert!(is_valid_pane_id_env(&first));
        assert!(is_valid_pane_id_env(&second));
    }

    #[test]
    fn workflow_shape_requires_unique_roles_and_one_start() {
        let roles = vec![
            WorkflowRoleInput {
                role: "planner".into(),
                command: "codex --model gpt-5.6-sol".into(),
                start: true,
            },
            WorkflowRoleInput {
                role: "builder".into(),
                command: "codex --model gpt-5.6-sol".into(),
                start: false,
            },
        ];
        assert_eq!(
            validate_workflow_shape("loop", "/tmp/project", &roles, 50, 200).unwrap(),
            (50, 200)
        );
        let mut duplicate = roles.clone();
        duplicate[1].role = "planner".into();
        assert!(validate_workflow_shape("loop", "/tmp/project", &duplicate, 50, 200).is_err());
        let mut no_start = roles;
        no_start[0].start = false;
        assert!(validate_workflow_shape("loop", "/tmp/project", &no_start, 50, 200).is_err());
    }

    #[test]
    fn desktop_workflow_platform_guard_blocks_windows_only() {
        assert!(ensure_desktop_workflow_platform_supported("macos").is_ok());
        assert!(ensure_desktop_workflow_platform_supported("linux").is_ok());
        let error = ensure_desktop_workflow_platform_supported("windows").unwrap_err();
        assert!(error.contains("unavailable on Windows"));
        assert!(error.contains("POSIX-shell quoted"));
    }
}
