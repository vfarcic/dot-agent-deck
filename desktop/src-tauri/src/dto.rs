use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dot_agent_deck::agent_pty::{
    AgentRecord, TabMembership, clamp_pty_dims, is_valid_cwd, is_valid_display_name,
    is_valid_orchestration_cwd, is_valid_pane_id_env,
};
use dot_agent_deck::agent_registry;
use dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;
use dot_agent_deck::event::{AgentType, SendResult, Writable};
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
    /// True when the ONLY thing that failed the handshake is the git-describe
    /// build stamp — the protocol version agreed on both sides — so the webview
    /// may offer Connect anyway (issue #801).
    ///
    /// Always emitted, including as `false`, because the webview branches on it
    /// to decide whether an override exists: an absent field and an incompatible
    /// wire must not look the same. A protocol mismatch never sets it, which is
    /// what keeps the wire check unoverridable from the UI as well as from the
    /// bypass itself.
    pub build_stamp_mismatch_only: bool,
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
    /// The BINARY the registry says this agent type runs — `claude`,
    /// `opencode`, `pi`, `codex`, `devin` — read straight off
    /// `AgentSpec::default_command` rather than restated here.
    ///
    /// `agent_type` above is the wire IDENTITY and stays snake_case for the
    /// consumers that key on it; it is not a name anybody types. Rendering it
    /// showed Claude Code as `claude_code` and OpenCode as `open_code`, with
    /// `codex` right only by coincidence. Deriving the answer from the registry
    /// is what stops a second copy of it drifting: adding an agent there gives
    /// this field its value with nothing to update here.
    ///
    /// Absent — never a placeholder, and never invented — for
    /// [`AgentType::None`] and for a record that reported no type at all.
    /// `None` is also `#[serde(other)]`, so a FUTURE agent type from a newer
    /// daemon lands there; this build genuinely does not know what binary that
    /// is, and says nothing rather than guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_name: Option<&'static str>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tool: Option<DesktopActiveTool>,
    pub tool_count: u32,
    /// The daemon's `SessionSnapshot.last_user_prompt` (PRD #745 M8) — the most
    /// recent prompt the operator sent this agent, and the honest answer to
    /// "what was this one asked to do".
    ///
    /// Absent, never a placeholder: an agent that has emitted no prompt event,
    /// a record with no `live` snapshot, and an older daemon all yield `None`,
    /// and `skip_serializing_if` keeps the key off the wire so the webview sees
    /// absence rather than an empty string it would have to special-case. The
    /// value is already control-stripped and byte-bounded by
    /// `daemon_client::sanitize_record_tab_membership`; the webview bounds and
    /// sanitises its own DISPLAY copy again at the render seam, because that
    /// scrub covers category `Cc` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_user_prompt: Option<String>,
    /// Whether the daemon can deliver input to this agent right now, projected
    /// from `SessionSnapshot.live_target.writable` (PRD #745 M8). Replaces the
    /// webview's hardcoded `"unknown"`.
    ///
    /// Only the `writable` half is surfaced: the deck's model speaks
    /// read/write/none, and `TargetKind` (pty / tmux / sdk / process) is a
    /// daemon-side implementation detail no desktop surface consumes. Absent
    /// when the daemon declared no live target at all — which the TUI reads as
    /// the legacy live default, so the desktop must NOT read absence as
    /// "read-only".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_lease: Option<&'static str>,
    /// When the daemon last saw this agent do anything, as epoch milliseconds
    /// (`SessionSnapshot.last_activity_ms`, PRD #745 M9). Copied through
    /// unchanged: the daemon owns the instant, the webview owns the wording.
    ///
    /// Absent when the record carries no `live` snapshot — a daemon that
    /// restarted has no sessions, so it reports no activity times rather than
    /// resetting them all to "just now" — and absent from an older daemon that
    /// predates the field. Absence renders as nothing, never a placeholder.
    ///
    /// NOT clamped here. The daemon's value is producer-supplied and can land
    /// in the future, and the only seam that can decide what to do about that
    /// is the one that owns the OTHER clock: the webview compares it against
    /// its own `Date.now()`, absorbs ordinary skew, and refuses to relativise
    /// anything beyond it. Clamping here against the desktop process's clock
    /// would silently move a value the daemon reported, using a third clock
    /// that is not the one the comparison is made on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<i64>,
    /// When the daemon spawned this agent's process, as epoch milliseconds
    /// (`AgentRecord.spawned_at_ms`, PRD #745 M11). Copied through unchanged,
    /// same split as `last_activity_ms`: the daemon owns the instant, the
    /// webview owns the wording.
    ///
    /// It comes off the REGISTRY record rather than the live session, which is
    /// what makes it answer a question `last_activity_ms` cannot: a session
    /// exists only once a hook event has arrived, so an agent that has never
    /// emitted one still reports a spawn time. Absent when the daemon did not
    /// spawn the process it is describing (an id-only reply from an older
    /// daemon) or predates the field — and absent means the column renders
    /// nothing, never a placeholder.
    ///
    /// NOT clamped here, for the reason spelled out on `last_activity_ms`: the
    /// desktop process holds a third clock, and the seam that owns the one the
    /// comparison is actually made against is the webview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_at_ms: Option<i64>,
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
        /// The orchestration TAB's own working directory
        /// (`TabMembership::orchestration_cwd`), shared by every role pane in
        /// the tab and distinct from each pane's own `cwd` — an orchestrator
        /// and its workers may sit in different per-pane directories while
        /// belonging to one orchestration (PRD #745 M8). The overview states it
        /// once in the group header, which is what turns the per-row column
        /// into a differences column. `None` when the daemon reported none.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
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
    /// Relax the build-stamp comparison for the rest of this app session and
    /// hand back a freshly classified snapshot (issue #801). Carries no
    /// payload: it is an assertion by the user, not a parameter, and it can
    /// only ever relax the stamp check — the protocol check runs first and is
    /// never bypassed.
    AllowBuildMismatch,
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

/// The orchestration arm deliberately binds EVERY field rather than ending in a
/// `..` rest pattern. The rest pattern is what silently swallowed
/// `orchestration_cwd` for as long as this function has existed (PRD #745 M8):
/// the daemon sent it, the desktop parsed it, and nothing here copied it out.
/// Binding them all makes the next field added to `TabMembership` a compile
/// error at this seam instead of another quietly dropped column.
fn map_tab(tab: Option<&TabMembership>) -> DesktopTab {
    match tab {
        None => DesktopTab::Dashboard,
        Some(TabMembership::Mode { name }) => DesktopTab::Mode { name: name.clone() },
        Some(TabMembership::Orchestration {
            name,
            role_index,
            role_name,
            is_start_role,
            orchestration_cwd,
            display_title,
            orchestration_id,
        }) => DesktopTab::Orchestration {
            name: name.clone(),
            role_index: *role_index,
            role_name: role_name.clone(),
            is_start_role: *is_start_role,
            cwd: orchestration_cwd.clone(),
            display_title: display_title.clone(),
            orchestration_id: orchestration_id.clone(),
        },
    }
}

/// The webview's read/write/none vocabulary for a daemon `LiveTarget`.
///
/// Only [`Writable`] is consulted — it is the half that answers "can the deck
/// type into this pane right now". `Writable::None` is also serde's
/// forward-compat catch-all, so a `writable` value a future daemon invents
/// lands on the SAFE, non-writable answer rather than being dressed up as a
/// live target.
fn write_lease_name(writable: &Writable) -> &'static str {
    match writable {
        Writable::Live => "write",
        Writable::HistoryOnly => "read",
        Writable::None => "none",
    }
}

pub(crate) fn map_agent(record: AgentRecord) -> DesktopAgent {
    let live = record.live.as_ref();
    // Resolved ONCE, so the wire identity and the binary name can never
    // disagree about which agent this is.
    let reported_type = live
        .and_then(|snapshot| snapshot.agent_type.as_ref())
        .or(record.agent_type.as_ref());
    let agent_type = reported_type
        .map(agent_type_name)
        .unwrap_or("none")
        .to_string();
    // Off the registry, which is where the deck already keeps the command that
    // launches each agent — see `cli_name`.
    let cli_name =
        reported_type.and_then(|agent_type| agent_registry::spec(agent_type).default_command);
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
    let last_user_prompt = live.and_then(|snapshot| snapshot.last_user_prompt.clone());
    let write_lease = live
        .and_then(|snapshot| snapshot.live_target.as_ref())
        .map(|target| write_lease_name(&target.writable));
    let last_activity_ms = live.and_then(|snapshot| snapshot.last_activity_ms);
    // PRD #745 M11: off the RECORD, not the live snapshot — the daemon knows
    // when it spawned a process whether or not that process has ever emitted an
    // event.
    let spawned_at_ms = record.spawned_at_ms;
    let tab = map_tab(record.tab_membership.as_ref());

    DesktopAgent {
        id: record.id,
        pane_id: record.pane_id_env,
        display_name: record.display_name,
        cwd: record.cwd,
        rows: record.rows,
        cols: record.cols,
        agent_type,
        cli_name,
        status,
        active_tool,
        tool_count,
        last_user_prompt,
        write_lease,
        last_activity_ms,
        spawned_at_ms,
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
            build_stamp_mismatch_only: false,
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
    // Issue #747: through the shared helper rather than a second copy of the
    // `.min()` pair, so the desktop bridge cannot drift from the TUI and the
    // daemon about what geometry a resize request actually produces.
    Ok(clamp_pty_dims(rows, cols))
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
    use dot_agent_deck::agent_pty::PTY_RESIZE_DIM_MAX;
    use dot_agent_deck::event::{LiveTarget, TargetKind};
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
                last_activity_ms: None,
            }),
            spawned_at_ms: None,
        }
    }

    #[test]
    fn agent_mapping_is_frontend_stable() {
        let value = serde_json::to_value(map_agent(fixture_record())).unwrap();
        assert_eq!(value["id"], "agent-7");
        assert_eq!(value["paneId"], "pane-7");
        assert_eq!(value["agentType"], "codex");
        assert_eq!(value["cliName"], "codex");
        assert_eq!(value["status"], "working");
        assert_eq!(value["activeTool"]["name"], "shell");
        assert_eq!(value["tab"]["kind"], "mode");
    }

    /// PRD #745: the CLI column shows a binary somebody could type, and EVERY
    /// variant is checked here rather than only the one the fixture happens to
    /// carry — the enum names had Claude Code reading `claude_code` and
    /// OpenCode reading `open_code`, with `codex` right only by coincidence.
    #[test]
    fn every_agent_type_reports_the_binary_it_runs() {
        let cli_of = |agent_type: Option<AgentType>| {
            let mut record = fixture_record();
            record.agent_type = agent_type;
            record.live = None;
            map_agent(record).cli_name
        };
        assert_eq!(cli_of(Some(AgentType::ClaudeCode)), Some("claude"));
        assert_eq!(cli_of(Some(AgentType::OpenCode)), Some("opencode"));
        assert_eq!(cli_of(Some(AgentType::Pi)), Some("pi"));
        assert_eq!(cli_of(Some(AgentType::Codex)), Some("codex"));
        assert_eq!(cli_of(Some(AgentType::Devin)), Some("devin"));
        // `None` is both "no recognized agent" and the `#[serde(other)]`
        // landing spot for a type this build has never heard of. Neither has a
        // binary this build can name, so it reports nothing rather than a word.
        assert_eq!(cli_of(Some(AgentType::None)), None);
        assert_eq!(cli_of(None), None);
    }

    /// The live snapshot's type wins over the record's for the binary name
    /// exactly as it does for the wire identity — one resolution, so the two
    /// fields cannot end up naming different agents.
    #[test]
    fn the_live_agent_type_decides_the_binary_too() {
        let mut record = fixture_record();
        record.agent_type = Some(AgentType::Codex);
        if let Some(live) = record.live.as_mut() {
            live.agent_type = Some(AgentType::ClaudeCode);
        }
        let mapped = map_agent(record);
        assert_eq!(mapped.agent_type, "claude_code");
        assert_eq!(mapped.cli_name, Some("claude"));
    }

    /// Absence stays OFF the wire, so the webview reads absence rather than an
    /// empty string it would have to special-case.
    #[test]
    fn an_unnameable_cli_is_absent_from_the_serialized_shape() {
        let mut record = fixture_record();
        record.agent_type = Some(AgentType::None);
        record.live = None;
        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["agentType"], "none");
        assert!(value.get("cliName").is_none());
    }

    #[test]
    fn record_without_hook_state_is_still_running() {
        let mut record = fixture_record();
        record.live = None;
        let mapped = map_agent(record);
        assert_eq!(mapped.status, "running");
        assert_eq!(mapped.tool_count, 0);
        assert!(mapped.active_tool.is_none());
        // PRD #745 M8: no live snapshot means no prompt and no lease to report.
        // Absent, not blank — the webview must be able to tell "nothing to say"
        // from "the daemon said the empty string".
        assert!(mapped.last_user_prompt.is_none());
        assert!(mapped.write_lease.is_none());
        // PRD #745 M9: nor an activity time. This is the case a RESTARTED
        // daemon produces for every agent — it persists no `AppState`, so it
        // has no sessions to snapshot — and it is exactly why this field could
        // be shipped where session duration could not: the honest answer to
        // "when did this last do something" after a restart is "I do not
        // know", and absence says that.
        assert!(mapped.last_activity_ms.is_none());
        // PRD #745 M11: the spawn time is INDEPENDENT of `live`, so removing
        // the snapshot must not be what makes it absent — this fixture is
        // absent because the record itself reports no spawn (see
        // `agent_mapping_surfaces_the_spawn_instant_unchanged` for the present
        // case, which keeps `live` untouched).
        assert!(mapped.spawned_at_ms.is_none());
    }

    /// PRD #745 M8: the two `SessionSnapshot` fields the desktop's own DTO used
    /// to drop even though the daemon sends them and the desktop parses them.
    #[test]
    fn agent_mapping_surfaces_the_last_prompt_and_the_write_lease() {
        let mut record = fixture_record();
        let live = record.live.as_mut().unwrap();
        live.last_user_prompt = Some("ship the overview".into());
        live.live_target = Some(LiveTarget {
            kind: TargetKind::Pty,
            writable: Writable::Live,
        });

        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["lastUserPrompt"], "ship the overview");
        assert_eq!(value["writeLease"], "write");
    }

    /// PRD #745 M9: the daemon's `last_activity_ms` reaches the webview as the
    /// same integer, under `lastActivityMs`, with no reformatting and no clamp.
    ///
    /// Pinned on the SERIALIZED value because a `serde_json` number is where a
    /// silent unit change would show up — seconds instead of milliseconds
    /// divides it by a thousand and every relative time on the overview becomes
    /// fifty-seven years, which is why the field carries its unit in its name.
    #[test]
    fn agent_mapping_surfaces_the_last_activity_instant_unchanged() {
        let mut record = fixture_record();
        record.live.as_mut().unwrap().last_activity_ms = Some(1_756_684_800_123);

        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["lastActivityMs"], 1_756_684_800_123i64);
    }

    /// The value is NOT clamped or validated here, deliberately: the daemon's
    /// `last_activity` is producer-supplied and can land in the future, and the
    /// only seam that can judge that is the one holding the other clock. A
    /// far-future instant therefore crosses this boundary intact, and the
    /// webview is what refuses to relativise it.
    #[test]
    fn a_future_last_activity_crosses_the_dto_boundary_intact() {
        let far_future = 4_102_444_800_000i64; // 2100-01-01T00:00:00Z
        let mut record = fixture_record();
        record.live.as_mut().unwrap().last_activity_ms = Some(far_future);

        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["lastActivityMs"], far_future);
    }

    /// PRD #745 M11: the daemon's `spawned_at_ms` reaches the webview as the
    /// same integer, under `spawnedAtMs`, with no reformatting and no clamp —
    /// and it comes off the RECORD, so it survives a record carrying no live
    /// session at all.
    ///
    /// That last half is the whole reason spawn time beats
    /// `SessionState.started_at`: a session exists only once a hook event has
    /// arrived, so an agent that has never emitted one has no start instant —
    /// and it is exactly the agent whose uptime a reader most wants. Pinned on
    /// the SERIALIZED value for the same reason M9's is: a seconds/milliseconds
    /// slip is a ×1000 error, which is why the unit is in the name.
    #[test]
    fn agent_mapping_surfaces_the_spawn_instant_unchanged() {
        let mut record = fixture_record();
        record.spawned_at_ms = Some(1_756_684_800_123);

        let value = serde_json::to_value(map_agent(record.clone())).unwrap();
        assert_eq!(value["spawnedAtMs"], 1_756_684_800_123i64);

        // No live snapshot, same answer: the daemon knows when it forked a
        // process whether or not that process has ever reported anything.
        record.live = None;
        let value = serde_json::to_value(map_agent(record)).unwrap();
        assert_eq!(value["spawnedAtMs"], 1_756_684_800_123i64);
        assert!(value.get("lastActivityMs").is_none());
    }

    /// The absent case for both, pinned in the SERIALIZED shape: the keys are
    /// missing from the JSON the webview receives rather than present and null,
    /// so `agent.lastUserPrompt` is `undefined` there and absence survives the
    /// boundary.
    #[test]
    fn absent_prompt_and_lease_are_omitted_from_the_frontend_shape() {
        let value = serde_json::to_value(map_agent(fixture_record())).unwrap();
        assert!(value.get("lastUserPrompt").is_none());
        assert!(value.get("writeLease").is_none());
        // PRD #745 M9, same rule: no key at all, so `agent.lastActivityMs` is
        // `undefined` in the webview and the column renders nothing.
        assert!(value.get("lastActivityMs").is_none());
        // PRD #745 M11, same rule again: a daemon that did not spawn the agent
        // — or one predating the field — sends no key, and the uptime column
        // renders nothing rather than a fabricated age.
        assert!(value.get("spawnedAtMs").is_none());
    }

    /// Only `Writable` decides the lease, and its `#[serde(other)]` catch-all
    /// means an unknown future value arrives as `None` — so the non-writable
    /// answer is what a daemon this build does not understand produces.
    #[test]
    fn write_lease_projects_every_writable_value() {
        let lease = |writable| {
            let mut record = fixture_record();
            record.live.as_mut().unwrap().live_target = Some(LiveTarget {
                kind: TargetKind::Tmux,
                writable,
            });
            map_agent(record).write_lease
        };
        assert_eq!(lease(Writable::Live), Some("write"));
        assert_eq!(lease(Writable::HistoryOnly), Some("read"));
        assert_eq!(lease(Writable::None), Some("none"));
    }

    /// PRD #745 M8: the orchestration tab's own cwd, which the `..` rest
    /// pattern in `map_tab` used to swallow.
    #[test]
    fn orchestration_tab_carries_the_orchestration_cwd() {
        let tab = |orchestration_cwd| {
            serde_json::to_value(map_tab(Some(&TabMembership::Orchestration {
                name: "dot-agent-deck".into(),
                role_index: 1,
                role_name: "coder".into(),
                is_start_role: false,
                orchestration_cwd,
                display_title: Some("PRD #745".into()),
                orchestration_id: Some("orc-745".into()),
            })))
            .unwrap()
        };

        let reported = tab(Some("/home/dev/code/dot-agent-deck".into()));
        assert_eq!(reported["kind"], "orchestration");
        assert_eq!(reported["cwd"], "/home/dev/code/dot-agent-deck");
        assert_eq!(reported["roleName"], "coder");
        assert_eq!(reported["orchestrationId"], "orc-745");

        // Absent stays absent: an orchestration whose cwd the daemon did not
        // report has no key at all, so the group header states nothing rather
        // than a placeholder.
        assert!(tab(None).get("cwd").is_none());
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
    fn allow_build_mismatch_action_carries_no_payload() {
        let action: DesktopAction = serde_json::from_value(serde_json::json!({
            "type": "allow_build_mismatch"
        }))
        .unwrap();
        assert!(matches!(action, DesktopAction::AllowBuildMismatch));
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
        // No daemon answered, so there is no stamp-only mismatch to override —
        // and the field is PRESENT rather than absent, because the webview
        // branches on it (issue #801).
        assert_eq!(value["connection"]["buildStampMismatchOnly"], false);
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
