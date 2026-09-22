//! PRD #1223 M2: what the daemon tells a new-agent form about the deck it will
//! start the agent on — what [`crate::daemon_protocol::AttachRequest::NewAgentOptions`]
//! answers.
//!
//! The desktop's form needs four facts the TUI reads from its own process, and
//! on a remote deck the desktop's process is the wrong place to read any of
//! them: the configured default command lives in a file on the deck's host, the
//! agent registry is whichever one the deck's build compiled in, and the
//! experimental flag has meaning where the spawn happens. So the daemon answers
//! for itself rather than the desktop computing an answer from its own build.

use serde::{Deserialize, Serialize};

use crate::config::DashboardConfig;

/// PRD #1223 M2: the daemon's reply to
/// [`crate::daemon_protocol::AttachRequest::NewAgentOptions`], carried on
/// [`crate::daemon_protocol::AttachResponse::new_agent_options`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAgentOptions {
    /// `DashboardConfig.default_command` from the configuration file on the
    /// daemon's host — the file the TUI reads there, `DOT_AGENT_DECK_CONFIG`
    /// included. Absent when that value is empty, which is the unconfigured
    /// default; otherwise verbatim, as the TUI's own prefill uses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_command: Option<String>,
    /// The agent registry this daemon was built with
    /// ([`crate::agent_registry::ALL`]), in registry order.
    #[serde(default)]
    pub agents: Vec<AgentOption>,
    /// The daemon process's own experimental-flag state
    /// ([`crate::features::experimental_enabled`]). A client uses it to decide
    /// which surfaces to show for this deck; the daemon branches on nothing
    /// here.
    #[serde(default)]
    pub experimental: bool,
    /// The authoring kinds this daemon can compose a seed for — the values
    /// `StartAgent.authoring_kind` accepts ([`crate::authoring_seeds::AuthoringKind::ALL`],
    /// in the TUI Mode cycler's order). Every kind this build knows, whatever
    /// the experimental flag says: the flag decides what a client SHOWS, which
    /// is the client's call (the desktop hides `schedule-issues` unless
    /// [`Self::experimental`] is true, as the TUI hides its chip), not what the
    /// daemon can do. Empty from a daemon predating PRD #1223 M7, which is the
    /// honest answer from one that cannot.
    #[serde(default)]
    pub authoring_kinds: Vec<String>,
}

/// One agent in [`NewAgentOptions::agents`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOption {
    /// The registry entry's first `detect_basenames` value (`claude`,
    /// `opencode`, …) — a stable key, where the display name is prose.
    pub id: String,
    /// `AgentSpec.label`.
    pub display_name: String,
    /// `AgentSpec.default_command`: the command the form's Agent picker writes
    /// into Command when this agent is chosen.
    #[serde(default)]
    pub default_command: Option<String>,
}

/// The options this daemon process reports, read at the moment of the request.
///
/// **Blocking** — it reads the host's `config.toml` — so the dispatch runs it on
/// a blocking thread. Read per request rather than cached: `DashboardConfig` has
/// no reload path in the daemon, and the TUI re-reads the same file whenever it
/// starts, so a cache here would be the one reader that could go stale.
pub fn for_this_daemon() -> NewAgentOptions {
    compose(
        &DashboardConfig::load(),
        crate::features::experimental_enabled(),
    )
}

/// [`for_this_daemon`]'s projection, with its two inputs passed in so it is
/// testable without a config file or the process-global flag.
pub fn compose(config: &DashboardConfig, experimental: bool) -> NewAgentOptions {
    NewAgentOptions {
        default_command: Some(config.default_command.clone()).filter(|c| !c.is_empty()),
        agents: registry_agents(),
        experimental,
        authoring_kinds: crate::authoring_seeds::AuthoringKind::ALL
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect(),
    }
}

/// [`crate::agent_registry::ALL`] projected onto the wire, in registry order.
///
/// An entry with no `detect_basenames` has no stable id and is left out rather
/// than sent with an invented one. No shipped entry is in that position — the
/// neutral `NONE` placeholder has none, and it is not in `ALL`.
pub fn registry_agents() -> Vec<AgentOption> {
    crate::agent_registry::ALL
        .iter()
        .filter_map(|spec| {
            Some(AgentOption {
                id: (*spec.detect_basenames.first()?).to_string(),
                display_name: spec.label.to_string(),
                default_command: spec.default_command.map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(default_command: &str) -> DashboardConfig {
        DashboardConfig {
            default_command: default_command.to_string(),
            ..DashboardConfig::default()
        }
    }

    #[test]
    fn the_registry_is_projected_in_order_with_its_first_basename_as_the_id() {
        let agents = registry_agents();
        assert_eq!(agents.len(), crate::agent_registry::ALL.len());
        for (agent, spec) in agents.iter().zip(crate::agent_registry::ALL) {
            assert_eq!(agent.id, spec.detect_basenames[0]);
            assert_eq!(agent.display_name, spec.label);
            assert_eq!(agent.default_command.as_deref(), spec.default_command);
        }
        assert_eq!(
            agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["claude", "opencode", "pi", "codex", "devin"]
        );
    }

    #[test]
    fn an_empty_default_command_is_absent_and_a_set_one_is_verbatim() {
        assert_eq!(compose(&config_with(""), false).default_command, None);
        assert_eq!(
            compose(&config_with("opencode --model x"), false)
                .default_command
                .as_deref(),
            Some("opencode --model x")
        );
    }

    #[test]
    fn the_flag_is_reported_as_given_and_every_authoring_kind_is_offered_either_way() {
        for experimental in [false, true] {
            let options = compose(&config_with("claude"), experimental);
            assert_eq!(options.experimental, experimental);
            assert_eq!(
                options.authoring_kinds,
                ["schedule", "schedule-issues", "dispatcher"],
                "PRD #1223 M7: the daemon lists every kind it can compose; filtering \
                 `schedule-issues` on the flag is the client's job"
            );
        }
    }

    #[test]
    fn the_wire_shape_lists_the_authoring_kinds_and_omits_an_absent_command() {
        let json = serde_json::to_value(compose(&config_with(""), false)).unwrap();
        assert!(json.get("default_command").is_none());
        assert_eq!(
            json["authoring_kinds"],
            serde_json::json!(["schedule", "schedule-issues", "dispatcher"])
        );
        assert_eq!(json["experimental"], serde_json::json!(false));
        assert_eq!(
            json["agents"][0],
            serde_json::json!({
                "id": "claude",
                "display_name": crate::agent_registry::CLAUDE_CODE.label,
                "default_command": "claude",
            })
        );
        let back: NewAgentOptions = serde_json::from_value(json).unwrap();
        assert_eq!(back, compose(&config_with(""), false));
    }
}
