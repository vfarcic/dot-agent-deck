//! Per-agent snapshot records the daemon echoes back over `list-agents`.
//!
//! Extracted from the binary's `agent_pty` module (PRD #176 M1.1). Only the
//! wire-shaped data types live here; the daemon-side validation helpers
//! (`is_valid_display_name`, `validate_tab_membership`, …) and the PTY registry
//! stay in the binary.

use serde::{Deserialize, Serialize};

use crate::event::AgentType;

/// Which tab a daemon-tracked agent pane belonged to at spawn time
/// (PRD #76 M2.12). Echoed back via `list_agents` so the TUI can rebuild
/// the user's mode/orchestration tab structure on reconnect instead of
/// stranding every hydrated pane on the dashboard.
///
/// Validation: the embedded `name` follows the same `is_valid_display_name`
/// grammar as `display_name` — non-empty, ≤ 128 bytes, no control bytes.
/// Anything failing that is dropped to `None` on capture so a buggy or
/// hostile same-user peer reaching the attach socket can't smuggle ANSI
/// escapes back via `list_agents` (the auditor-flagged echo path).
///
/// Wire shape (serde):
/// ```json
/// { "kind": "mode", "name": "k8s-ops" }
/// { "kind": "orchestration", "name": "tdd-cycle", "role_index": 2 }
/// ```
///
/// `kind` tag is `snake_case` to match the other JSON enums in this crate.
/// `Option<TabMembership>` on `AgentRecord` / `StartAgent` is serialized with
/// `skip_serializing_if = "Option::is_none"` so older clients/daemons keep
/// working: a daemon predating this field sends nothing, and a TUI predating
/// this field ignores any extra key. `None` is the dashboard pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabMembership {
    /// Agent pane of a Mode tab. Side panes (the cards on the left) are
    /// NOT daemon-tracked — they respawn fresh from `ModeConfig.panes` on
    /// reconnect, see PRD #76 M2.12 design decision 2.
    Mode { name: String },
    /// One role slot of an orchestration tab. `role_index` is the position
    /// of this role in `OrchestrationConfig.roles`; on reconnect a dead
    /// slot (between role-index 0 and `roles.len()` with no surviving
    /// agent) is marked failed rather than respawned.
    ///
    /// PRD #93 round-5: the daemon now owns the orchestration dispatch flow
    /// (delegate / work-done) and writes the per-role prompt directly into
    /// the target pane's PTY. To do that without needing to load the
    /// orchestration config on the daemon side, `role_name` and
    /// `is_start_role` are carried inline alongside the index: `role_name`
    /// populates the daemon's `pane_role_map` and `is_start_role` populates
    /// its `orchestrator_pane_ids` set.
    Orchestration {
        name: String,
        role_index: usize,
        #[serde(default)]
        role_name: String,
        #[serde(default)]
        is_start_role: bool,
        /// Round-11 auditor #C: the absolute cwd of the orchestration
        /// tab, shared across every role pane in the same orchestration.
        /// Used as a disambiguator in `pane_orchestration_map` so two
        /// unnamed orchestrations whose cwd-basenames collide (e.g.
        /// `~/a/foo` and `~/b/foo`) get distinct identities. Distinct
        /// from each pane's own per-pane cwd: orchestrator and workers
        /// may have different per-pane cwds (PRD #93 round-9 #2) but
        /// they share one orchestration_cwd because they belong to the
        /// same tab. `Option<String>` with `#[serde(default)]` so an
        /// older client/daemon that omits the field still parses.
        /// `None` means "no disambiguator" — the lookup falls back to
        /// name-only, matching the pre-round-11 behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orchestration_cwd: Option<String>,
        /// PRD #107 follow-up: the user-typed orchestration name from the
        /// new-pane form. Carried through the daemon round-trip so a
        /// detach/reattach restores the displayed tab TITLE instead of
        /// recomputing it from `resolve_orchestration_name` (config name or
        /// cwd basename). The orchestration IDENTITY stays in `name` — this
        /// is title-only and never feeds delegate/role lookups. `None` (the
        /// common case for daemon-initiated/scheduled orchestrations and
        /// older clients) means the title falls back to the canonical name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_title: Option<String>,
    },
}

impl TabMembership {
    /// Borrow the tab name (mode or orchestration) so callers don't have
    /// to match on the variant for the common "extract name for validation
    /// or lookup" case.
    pub fn name(&self) -> &str {
        match self {
            TabMembership::Mode { name } => name,
            TabMembership::Orchestration { name, .. } => name,
        }
    }
}

/// Snapshot of one daemon-side agent that the M2.x rehydration path needs.
/// Carries the registry id plus the spawn-time `DOT_AGENT_DECK_PANE_ID`
/// captured in the daemon's `RunningAgent.pane_id_env`, so the TUI can rebuild
/// its pane→agent mapping using the *same* pane id the agent's child process
/// already carries in its environment. Also doubles as the wire-format
/// element for `AttachResponse::agent_records` — serde derives live here
/// so the in-memory and over-the-wire shapes can't drift apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id_env: Option<String>,
    /// Display name as last set on the daemon (M2.11). `None` means either
    /// the agent was spawned without a label or the value failed
    /// `is_valid_display_name` validation. `skip_serializing_if` keeps
    /// the wire shape backwards-compatible with older clients that don't
    /// know about this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Working directory the agent was launched in, if recorded (M2.11).
    /// `None` when neither the original spawn nor a later `SetAgentLabel`
    /// supplied a value, or when the supplied value failed `is_valid_cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Which tab this pane belonged to at spawn time (PRD #76 M2.12).
    /// `None` means either the agent was a dashboard pane, the spawn
    /// supplied an invalid value (dropped at capture), or the daemon ran
    /// an older binary that didn't persist this field. The TUI uses this
    /// to rebuild mode/orchestration tabs on reconnect.
    /// `skip_serializing_if` keeps the wire shape backwards-compatible
    /// with daemons predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_membership: Option<TabMembership>,
    /// Which AI agent this pane was spawned to run (PRD #76 M2.13).
    /// `None` means either the spawn didn't supply a recognized agent
    /// command, the pane is non-agent, or the daemon ran an older binary
    /// that didn't persist this field. The TUI uses this on reconnect to
    /// populate the placeholder session's `agent_type` (otherwise the
    /// dashboard renders "No agent" until a `SessionStart` hook fires).
    /// `skip_serializing_if` keeps the wire shape backwards-compatible
    /// with daemons predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<AgentType>,
    /// Current PTY rows as last opened or resized on the daemon (PRD
    /// #104). Threaded into the client's vt100 parser at hydration so
    /// snapshot bytes are parsed at the dims they were written at —
    /// without this, a wide-PTY agent's scrollback was clamped to
    /// 80 columns on reattach and the historical rows were corrupted.
    ///
    /// `#[serde(default)]` keeps the wire shape backwards-compatible
    /// for *decode*: an older daemon that omits the field round-trips
    /// as `0`, and the hydration path falls back to the 24×80
    /// placeholder when it sees `0`.
    ///
    /// `skip_serializing_if = "is_zero_u16"` (PRD #104 RN1, reviewer):
    /// on the *encode* side, a daemon that has no real dims yet (e.g.
    /// a future code path that constructs an `AgentRecord` before the
    /// PTY is open) emits the legacy shape — no `rows`/`cols` keys —
    /// instead of the new-shape literal `0`. Pre-PRD clients see the
    /// same JSON they always have; post-PRD clients decode the absent
    /// field via `#[serde(default)]`. Symmetric with the
    /// `pane_id_env` / `display_name` / `cwd` / `tab_membership` /
    /// `agent_type` fields that already use this pattern.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub rows: u16,
    /// Current PTY cols. See `rows` for the full rationale.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub cols: u16,
}

/// Skip-predicate for `AgentRecord::rows` / `AgentRecord::cols`
/// serialization. Pulled out as a named helper so the two `#[serde]`
/// attributes share one symbol — closure literals aren't allowed in
/// `skip_serializing_if`.
fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_membership_name_borrows_either_variant() {
        assert_eq!(
            TabMembership::Mode {
                name: "k8s-ops".into()
            }
            .name(),
            "k8s-ops"
        );
        assert_eq!(
            TabMembership::Orchestration {
                name: "tdd-cycle".into(),
                role_index: 0,
                role_name: String::new(),
                is_start_role: false,
                orchestration_cwd: None,
                display_title: None,
            }
            .name(),
            "tdd-cycle"
        );
    }

    #[test]
    fn agent_record_omits_rows_cols_when_zero_on_the_wire() {
        // PRD #104 RN1: a zero `rows`/`cols` must serialize to the legacy
        // shape (no keys) via `is_zero_u16`, and decode back to zero.
        let rec = AgentRecord {
            id: "1".into(),
            pane_id_env: None,
            display_name: None,
            cwd: None,
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("rows"), "rows=0 must be omitted");
        assert!(!obj.contains_key("cols"), "cols=0 must be omitted");

        let back: AgentRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back.rows, 0);
        assert_eq!(back.cols, 0);
    }

    #[test]
    fn agent_record_round_trips_nonzero_rows_cols() {
        let rec = AgentRecord {
            id: "7".into(),
            pane_id_env: Some("pid-7".into()),
            display_name: Some("coder".into()),
            cwd: Some("/work".into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            rows: 50,
            cols: 200,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: AgentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rows, 50);
        assert_eq!(back.cols, 200);
        assert_eq!(back.agent_type, Some(AgentType::ClaudeCode));
    }
}
