//! `dot-agent-deck daemon status [--json]`.
//!
//! Read-only diagnostic snapshot of the daemon's managed agents. A pure CLI
//! consumer of the existing `AttachRequest::ListAgents`: no new attach
//! request variant, no `PROTOCOL_VERSION` bump (see
//! `.dot-agent-deck/47-status-query-design.md` in the root checkout). This
//! module only reshapes `ListAgents`' `Vec<AgentRecord>` into the CLI's own
//! documented fields — it touches no daemon-side locking, since the existing
//! `ListAgents` handler (`daemon_protocol.rs`) already bounds itself to a
//! short `AppState` read lock released before any I/O await.
//!
//! Deliberately excluded from both the human table and the JSON document:
//! `last_user_prompt` / `first_prompts` (a status query must never surface
//! prompt text — see `daemon/status/004`) and `hook_session_id` /
//! `last_activity` (the design doc's proposed shape includes them, but
//! neither field currently exists on `SessionSnapshot`; adding them would
//! mean updating every existing `SessionSnapshot` struct literal across the
//! crate, including fixtures in `tests/rehydration.rs` this task must not
//! touch. Left as a deliberate follow-up rather than folded in here).

use std::time::Duration;

use serde::Serialize;

use crate::agent_pty::{AgentRecord, TabMembership};
use crate::state::{ActiveTool, SessionStatus};

/// Version of the `--json` document shape. Bump on a field removal or a
/// meaning change; additive fields don't need a bump — consumers should
/// tolerate unknown keys.
pub const SCHEMA_VERSION: u32 = 1;

/// Deadline for the whole connect+request round trip against the attach
/// socket (the design rationale: "cap the CLI's connect/request round trip
/// with a deadline ... [a timeout] must never retry in a loop or cause lazy
/// daemon startup"). This is one-shot, local Unix-socket IPC, so 3s
/// comfortably covers a live daemon under load without leaving a caller
/// stuck waiting on a wedged one.
pub const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// One row of the status output — the CLI's own documented shape, not a raw
/// re-export of `AgentRecord`/`SessionSnapshot`.
#[derive(Debug, Clone, Serialize)]
pub struct StatusAgent {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tool: Option<ActiveTool>,
}

/// Top-level `--json` document (the design rationale's proposed shape, minus
/// the fields explained in the module doc comment above).
#[derive(Debug, Clone, Serialize)]
pub struct StatusDocument {
    pub schema_version: u32,
    pub agents: Vec<StatusAgent>,
}

impl StatusDocument {
    pub fn new(agents: Vec<StatusAgent>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            agents,
        }
    }
}

/// `TabMembership` -> a short human role label. `None` (no membership, i.e.
/// a dashboard pane) stays `None` — the caller decides how to render that.
fn role_of(tab_membership: &Option<TabMembership>) -> Option<String> {
    match tab_membership {
        None => None,
        Some(TabMembership::Mode { name }) => Some(format!("mode:{name}")),
        Some(TabMembership::Orchestration {
            role_name,
            is_start_role,
            ..
        }) => {
            let role = if role_name.is_empty() {
                "role"
            } else {
                role_name.as_str()
            };
            if *is_start_role {
                Some(format!("{role} (orchestrator)"))
            } else {
                Some(role.to_string())
            }
        }
    }
}

/// Reduce the daemon's `ListAgents` reply to the CLI's own status shape.
/// Pure — no I/O — so it's unit-testable independent of a live daemon.
pub fn build_status_agents(records: Vec<AgentRecord>) -> Vec<StatusAgent> {
    records
        .into_iter()
        .map(|record| {
            let live = record.live;
            StatusAgent {
                agent_id: record.id,
                pane_id: record.pane_id_env,
                label: record.display_name,
                cwd: record.cwd,
                role: role_of(&record.tab_membership),
                status: live.as_ref().map(|s| s.status.clone()),
                active_tool: live.and_then(|s| s.active_tool),
            }
        })
        .collect()
}

const DASH: &str = "-";

fn cell(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or(DASH)
}

/// Render the concise, one-row-per-agent human table. Never includes prompt
/// text or scrollback (the design rationale) — only the diagnostic fields on
/// [`StatusAgent`] are candidates for a column here.
pub fn format_human(agents: &[StatusAgent]) -> String {
    if agents.is_empty() {
        return "no managed agents\n".to_string();
    }
    let mut out = String::new();
    out.push_str("PANE\tAGENT\tROLE\tSTATUS\tTOOL\tLABEL\tCWD\n");
    for a in agents {
        let status = a
            .status
            .as_ref()
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|| DASH.to_string());
        let tool = a
            .active_tool
            .as_ref()
            .map(|t| t.name.as_str())
            .unwrap_or(DASH);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            cell(&a.pane_id),
            a.agent_id,
            cell(&a.role),
            status,
            tool,
            cell(&a.label),
            cell(&a.cwd),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionSnapshot;

    fn record(id: &str, pane: &str, live: Option<SessionSnapshot>) -> AgentRecord {
        AgentRecord {
            id: id.to_string(),
            pane_id_env: Some(pane.to_string()),
            display_name: None,
            cwd: Some("/tmp/x".to_string()),
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
            live,
        }
    }

    fn snapshot(status: SessionStatus) -> SessionSnapshot {
        SessionSnapshot {
            status,
            agent_type: None,
            active_tool: None,
            tool_count: 0,
            first_prompts: Vec::new(),
            last_user_prompt: None,
            live_target: None,
        }
    }

    /// Scenario: build status rows from a driven agent (has a live
    /// `Thinking` snapshot) and an untouched control agent (no live
    /// snapshot). Confirm the rendered human table names both pane ids and
    /// that stripping each row's own pane id still leaves them different —
    /// this is the pure-function core of `daemon/status/001`.
    #[test]
    fn build_status_agents_distinguishes_driven_from_control() {
        let records = vec![
            record("agent-1", "driven", Some(snapshot(SessionStatus::Thinking))),
            record("agent-2", "control", None),
        ];
        let agents = build_status_agents(records);
        let table = format_human(&agents);
        assert!(table.contains("driven"));
        assert!(table.contains("control"));

        let driven_line = table.lines().find(|l| l.contains("driven")).unwrap();
        let control_line = table.lines().find(|l| l.contains("control")).unwrap();
        let driven_norm = driven_line
            .replace("driven", "<pane>")
            .replace("agent-1", "<agent>");
        let control_norm = control_line
            .replace("control", "<pane>")
            .replace("agent-2", "<agent>");
        assert_ne!(driven_norm, control_norm);
    }

    /// Scenario: a status row built from a live snapshot carrying a seeded
    /// `last_user_prompt` must never leak that prompt text into either the
    /// human table or the JSON document — the pure-function core of
    /// `daemon/status/004`.
    #[test]
    fn format_human_and_json_never_include_prompt_text() {
        let mut snap = snapshot(SessionStatus::Working);
        snap.last_user_prompt = Some("SENTINEL-PROMPT-TEXT".to_string());
        snap.first_prompts = vec!["SENTINEL-PROMPT-TEXT".to_string()];
        let agents = build_status_agents(vec![record("agent-1", "leak", Some(snap))]);

        let table = format_human(&agents);
        assert!(!table.contains("SENTINEL-PROMPT-TEXT"));

        let json = serde_json::to_string(&StatusDocument::new(agents)).unwrap();
        assert!(!json.contains("SENTINEL-PROMPT-TEXT"));
    }

    #[test]
    fn json_document_carries_schema_version_and_pane_id() {
        let agents = build_status_agents(vec![record("agent-1", "json-pane", None)]);
        let json = serde_json::to_string(&StatusDocument::new(agents)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(json.contains("json-pane"));
    }
}
