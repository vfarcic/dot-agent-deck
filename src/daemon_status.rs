//! `dot-agent-deck daemon status [--json]`.
//!
//! Read-only diagnostic snapshot of the daemon's managed agents. A pure CLI
//! consumer of the existing `AttachRequest::ListAgents`: no new attach request
//! variant, and therefore no `PROTOCOL_VERSION` bump — the command adds nothing
//! to the TUI↔daemon wire, so an older daemon serves a newer CLI's status query
//! unchanged. (Issue #459: that rationale used to be a citation of a design note
//! under `.dot-agent-deck/`, which `.gitignore` excludes, so nobody reading the
//! merged source could follow it. The reasoning is inlined here instead.) This
//! module only reshapes `ListAgents`' `Vec<AgentRecord>` into the CLI's own
//! documented fields — it touches no daemon-side locking, since the existing
//! `ListAgents` handler (`daemon_protocol.rs`) already bounds itself to a
//! short `AppState` read lock released before any I/O await.
//!
//! Deliberately excluded from both the human table and the JSON document:
//!
//! * `last_user_prompt` / `first_prompts`. A status query is a diagnostic, run
//!   by anyone who can reach the socket and routinely pasted into a bug report
//!   or a terminal someone else is watching; prompt text is the user's private
//!   content and has no diagnostic value that the status/tool columns do not
//!   already carry. Pinned by `daemon/status/004`. The same reasoning is why
//!   [`StatusTool`] carries the tool NAME without its `detail` (issue #455).
//! * `hook_session_id` / `last_activity`. Wanted, but neither field exists on
//!   `SessionSnapshot` today; adding them means updating every
//!   `SessionSnapshot` struct literal across the crate. Left as a deliberate
//!   follow-up rather than folded in here.

use std::time::Duration;

use serde::Serialize;

use crate::agent_pty::{AgentRecord, TabMembership};
use crate::state::{ActiveTool, SessionStatus};

/// Version of the `--json` document shape. Bump on a field removal or a
/// meaning change; additive fields don't need a bump — consumers should
/// tolerate unknown keys.
///
/// `2` (issue #455): `active_tool.detail` was REMOVED. Version 1 serialized the
/// internal [`ActiveTool`] whole, so the JSON document carried the tool's
/// `detail` — the first line of the command being run, the file path being
/// read/written, the search pattern, the subagent description (see
/// `crate::hook`) — while the human table had always printed the tool NAME
/// only. Both modes now honour the same privacy contract, so a consumer that
/// read `active_tool.detail` gets nothing rather than something subtly
/// different: a field removal, which this constant exists to announce.
pub const SCHEMA_VERSION: u32 = 2;

/// Deadline for the whole connect+request round trip against the attach
/// socket. This is one-shot, local Unix-socket IPC, so 3s comfortably covers a
/// live daemon under load without leaving a caller stuck waiting on a wedged
/// one.
///
/// Expiry ABANDONS the query — it must never retry in a loop, and must never
/// cause lazy daemon startup. Both halves are load-bearing for a diagnostic
/// (issue #459 — inlined here rather than left as a citation of a note under
/// the gitignored `.dot-agent-deck/`): the command's whole job is to report the
/// daemon's condition without perturbing it, so a retry loop against a wedged
/// daemon would add load to the exact failure being diagnosed, and spawning a
/// daemon to answer "is a daemon running?" would make the question
/// unanswerable — the honest answer for an unreachable daemon is the
/// `unavailable` failure exit, not a freshly-minted empty one.
pub const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// The public projection of a live [`ActiveTool`]: the tool's NAME, and
/// nothing else.
///
/// Issue #455: this type exists so the privacy contract is enforced by the
/// type system rather than by remembering. `StatusAgent` used to carry
/// `Option<ActiveTool>` — the internal state type — so every field ever added
/// to `ActiveTool` rode into the `--json` document for free, which is exactly
/// how `detail` (a command line, a file path, a search pattern — see
/// `crate::hook`) leaked out of a command documented as never printing prompt
/// text. Adding a field to `ActiveTool` can no longer widen this document; it
/// takes a deliberate edit here, and that edit owes a [`SCHEMA_VERSION`] bump.
///
/// Kept as an object with a `name` key rather than flattened to a bare string
/// so the change is a pure field REMOVAL: a v1 consumer reading
/// `active_tool.name` still reads it under v2, and only `active_tool.detail`
/// disappears.
#[derive(Debug, Clone, Serialize)]
pub struct StatusTool {
    pub name: String,
}

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
    pub active_tool: Option<StatusTool>,
}

/// Top-level `--json` document: a [`SCHEMA_VERSION`] and the `agents` array,
/// each entry a [`StatusAgent`].
///
/// Issue #459 follow-through: this used to defer to "the design rationale",
/// which was a note under the gitignored `.dot-agent-deck/` — no reader of the
/// merged source could follow it. The shape is stated here instead, and what is
/// deliberately absent from it (`last_user_prompt` / `first_prompts`,
/// `hook_session_id` / `last_activity`) is stated in the module doc comment
/// above, with the reason for each.
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
                // Issue #455: project down to the NAME here, at the one place
                // that crosses from internal state into the CLI's document —
                // `detail` never leaves this function.
                active_tool: live
                    .and_then(|s| s.active_tool)
                    .map(|tool: ActiveTool| StatusTool { name: tool.name }),
            }
        })
        .collect()
}

const DASH: &str = "-";

fn cell(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or(DASH)
}

/// Render the concise, one-row-per-agent human table. Never includes prompt
/// text or scrollback (see the module docs for why) — only the diagnostic
/// fields on [`StatusAgent`] are candidates for a column here, and the TOOL
/// column deliberately shows the tool's name without its `detail`.
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
            spawned_at_ms: None,
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
            last_activity_ms: None,
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

    /// Scenario: a status row built from a live snapshot whose `active_tool`
    /// carries a `detail` must publish the tool's NAME and drop the detail —
    /// in the human table (which always did) and in the JSON document (which
    /// serialized `ActiveTool` whole until issue #455). The pure-function core
    /// of `daemon/status/004`'s `--json` half.
    #[test]
    fn active_tool_publishes_the_name_and_never_the_detail() {
        let mut snap = snapshot(SessionStatus::Working);
        snap.active_tool = Some(ActiveTool {
            name: "Read".to_string(),
            detail: Some("SENTINEL-TOOL-DETAIL".to_string()),
        });
        let agents = build_status_agents(vec![record("agent-1", "tool-pane", Some(snap))]);

        let table = format_human(&agents);
        assert!(
            table.contains("Read"),
            "table must name the tool: {table:?}"
        );
        assert!(!table.contains("SENTINEL-TOOL-DETAIL"));

        let json = serde_json::to_string(&StatusDocument::new(agents)).unwrap();
        assert!(
            !json.contains("SENTINEL-TOOL-DETAIL"),
            "json leaked: {json}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let tool = &parsed["agents"][0]["active_tool"];
        assert_eq!(tool["name"], "Read");
        assert!(
            tool.get("detail").is_none(),
            "`active_tool.detail` must be gone from the document, not merely \
             empty; got {tool:?}"
        );
    }

    #[test]
    fn json_document_carries_schema_version_and_pane_id() {
        let agents = build_status_agents(vec![record("agent-1", "json-pane", None)]);
        let json = serde_json::to_string(&StatusDocument::new(agents)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Version 2 (issue #455): `active_tool.detail` was removed from the
        // document, and `SCHEMA_VERSION`'s contract is to announce exactly
        // that.
        assert_eq!(parsed["schema_version"], 2);
        assert!(json.contains("json-pane"));
    }
}
