//! The environment an agent the deck starts receives, so its own telemetry
//! lands in the same backend tied to the same run.
//!
//! An agent inherits the daemon's environment (the spawn path scrubs a few deck
//! variables and overlays its own; it does not clear the environment), so the
//! user's `OTEL_*` configuration already reaches it unchanged. What this adds is
//! only what differs from that: `OTEL_RESOURCE_ATTRIBUTES` with the run's
//! identity merged in, and, for Claude Code, the switches that turn its
//! telemetry on — each one only when the user has not set it, and a signal's
//! exporter only when the user gave that signal somewhere to go: the general
//! endpoint or the signal's own. A traces-only configuration is the user's
//! choice of where traces go, not a destination for metrics and logs, which
//! would otherwise fall back to `http://localhost:4318`.
//!
//! Content switches (`OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_DETAILS` and their
//! like) are never set. A user who set one themselves still passes it through,
//! as they pass every other variable.

use crate::event::AgentType;

use super::config::{
    EnvMap, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_PROTOCOL, OTEL_RESOURCE_ATTRIBUTES,
    non_empty,
};
use super::resource::merge_resource_attributes;
use super::tracker::{OrchestrationInfo, agent_name};
use super::{AttrKey, SCHEMA_VERSION, TelemetryConfig};

/// Claude Code's signal exporters the deck switches on, each with the
/// endpoint variable for that signal alone. Either it or
/// `OTEL_EXPORTER_OTLP_ENDPOINT` must be set for the deck to switch it on.
pub const CLAUDE_CODE_SIGNALS: [(&str, &str, &str); 2] = [
    (
        "OTEL_METRICS_EXPORTER",
        "otlp",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    ),
    (
        "OTEL_LOGS_EXPORTER",
        "otlp",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    ),
];

/// Claude Code's own switch, set when the deck switches on one of
/// [`CLAUDE_CODE_SIGNALS`].
pub const CLAUDE_CODE_ENABLE_TELEMETRY: (&str, &str) = ("CLAUDE_CODE_ENABLE_TELEMETRY", "1");

/// The protocol the deck gives Claude Code when the user set none: the OTLP
/// default. Never the traces-only protocol, which describes the traces
/// endpoint and nothing else.
pub const CLAUDE_CODE_DEFAULT_PROTOCOL: &str = "http/protobuf";

/// Variables that make an agent's telemetry carry content. Listed so a test can
/// assert none of them is ever produced here.
#[cfg(test)]
pub const CONTENT_SWITCHES: [&str; 2] = ["OTEL_LOG_USER_PROMPTS", "OTEL_LOG_TOOL_DETAILS"];

/// Who a run is, as an agent's resource attributes describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity<'a> {
    pub run_id: &'a str,
    pub pane_id: &'a str,
    pub agent: &'a AgentType,
    pub orchestration: Option<&'a OrchestrationInfo>,
}

/// The variables to add to or override in an agent's environment.
///
/// `base_env` is what the agent would otherwise receive — the daemon's
/// environment with the spawn's own overlay applied — so a value a role's
/// configuration sets counts as set. Empty when `config` is `None`: with
/// telemetry off an agent's environment is exactly what it was before.
pub fn agent_env(
    config: Option<&TelemetryConfig>,
    base_env: &EnvMap,
    identity: &RunIdentity<'_>,
    host: Option<&str>,
) -> Vec<(String, String)> {
    if config.is_none() {
        return Vec::new();
    }
    let mut deck_pairs: Vec<(String, String)> = vec![
        (AttrKey::RunId.as_str().into(), identity.run_id.into()),
        (AttrKey::PaneId.as_str().into(), identity.pane_id.into()),
        (
            AttrKey::Agent.as_str().into(),
            agent_name(identity.agent).into(),
        ),
    ];
    if let Some(orch) = identity.orchestration {
        for (key, value) in [
            (AttrKey::Role, orch.role.as_deref()),
            (AttrKey::OrchestrationName, orch.name.as_deref()),
            (AttrKey::OrchestrationId, orch.id.as_deref()),
        ] {
            if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
                deck_pairs.push((key.as_str().into(), value.into()));
            }
        }
    }
    if let Some(host) = host.filter(|h| !h.is_empty()) {
        deck_pairs.push((AttrKey::HostName.as_str().into(), host.into()));
    }
    deck_pairs.push((
        AttrKey::SchemaVersion.as_str().into(),
        SCHEMA_VERSION.into(),
    ));

    let mut out = vec![(
        OTEL_RESOURCE_ATTRIBUTES.to_string(),
        merge_resource_attributes(non_empty(base_env, OTEL_RESOURCE_ATTRIBUTES), &deck_pairs),
    )];
    if *identity.agent == AgentType::ClaudeCode {
        out.extend(claude_code_switches(base_env));
    }
    out
}

/// The Claude Code switches to add to `base_env`: each signal exporter the
/// user left unset whose signal has an endpoint, and, when there is one, the
/// telemetry switch and the protocol, if the user left those unset too.
fn claude_code_switches(base_env: &EnvMap) -> Vec<(String, String)> {
    let unset = |key: &str| non_empty(base_env, key).is_none();
    let general = !unset(OTEL_EXPORTER_OTLP_ENDPOINT);
    let mut out: Vec<(String, String)> = CLAUDE_CODE_SIGNALS
        .into_iter()
        .filter(|(exporter, _, endpoint)| unset(exporter) && (general || !unset(endpoint)))
        .map(|(exporter, value, _)| (exporter.to_string(), value.to_string()))
        .collect();
    if out.is_empty() {
        return out;
    }
    for (key, value) in [
        CLAUDE_CODE_ENABLE_TELEMETRY,
        (OTEL_EXPORTER_OTLP_PROTOCOL, CLAUDE_CODE_DEFAULT_PROTOCOL),
    ] {
        if unset(key) {
            out.push((key.to_string(), value.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::config::OTEL_EXPORTER_OTLP_ENDPOINT;
    use crate::telemetry::resource::parse_resource_attributes;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn config(extra: &[(&str, &str)]) -> TelemetryConfig {
        let mut pairs = vec![(OTEL_EXPORTER_OTLP_ENDPOINT, "http://c:4318")];
        pairs.extend_from_slice(extra);
        TelemetryConfig::from_env(&env(&pairs)).unwrap()
    }

    fn identity<'a>(agent: &'a AgentType, orch: Option<&'a OrchestrationInfo>) -> RunIdentity<'a> {
        RunIdentity {
            run_id: "run-1",
            pane_id: "pane-7",
            agent,
            orchestration: orch,
        }
    }

    fn get<'a>(out: &'a [(String, String)], key: &str) -> Option<&'a str> {
        out.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn export_off_adds_nothing() {
        let base = env(&[(OTEL_RESOURCE_ATTRIBUTES, "team=x")]);
        assert!(
            agent_env(
                None,
                &base,
                &identity(&AgentType::ClaudeCode, None),
                Some("h")
            )
            .is_empty()
        );
    }

    #[test]
    fn claude_gets_identity_and_the_unset_switches_only() {
        let orch = OrchestrationInfo {
            role: Some("code reviewer".into()),
            name: Some("tdd".into()),
            id: Some("orch-1".into()),
            orchestrator: true,
        };
        let base = env(&[
            (OTEL_EXPORTER_OTLP_ENDPOINT, "http://c:4318"),
            (OTEL_RESOURCE_ATTRIBUTES, "team=platform,deck.run.id=stale"),
            ("OTEL_LOGS_EXPORTER", "none"),
            ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json"),
        ]);
        let out = agent_env(
            Some(&config(&[(
                "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
                "http/json",
            )])),
            &base,
            &identity(&AgentType::ClaudeCode, Some(&orch)),
            Some("studio"),
        );
        let attrs = parse_resource_attributes(get(&out, OTEL_RESOURCE_ATTRIBUTES).unwrap());
        let attr = |k: &str| {
            attrs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(attr("team"), Some("platform"));
        assert_eq!(attr("deck.run.id"), Some("run-1"));
        assert_eq!(attr("deck.pane.id"), Some("pane-7"));
        assert_eq!(attr("deck.agent"), Some("claude"));
        assert_eq!(attr("deck.role"), Some("code reviewer"));
        assert_eq!(attr("deck.orchestration.name"), Some("tdd"));
        assert_eq!(attr("deck.orchestration.id"), Some("orch-1"));
        assert_eq!(attr("host.name"), Some("studio"));
        assert_eq!(attr("deck.telemetry.schema_version"), Some("2"));
        assert!(
            get(&out, OTEL_RESOURCE_ATTRIBUTES)
                .unwrap()
                .contains("code%20reviewer"),
            "values are percent-encoded"
        );

        assert_eq!(get(&out, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some("1"));
        assert_eq!(get(&out, "OTEL_METRICS_EXPORTER"), Some("otlp"));
        assert_eq!(
            get(&out, "OTEL_LOGS_EXPORTER"),
            None,
            "the user's value stands"
        );
        assert_eq!(
            get(&out, OTEL_EXPORTER_OTLP_PROTOCOL),
            Some("http/protobuf"),
            "the default, not the traces-only protocol"
        );
        for key in CONTENT_SWITCHES {
            assert_eq!(get(&out, key), None);
        }
    }

    /// A traces-only endpoint sends Claude Code's metrics and logs nowhere the
    /// user named, so neither is switched on; each signal's own endpoint
    /// switches on that signal alone; and the protocol given is the user's
    /// general one or the default, never the traces-only one.
    #[test]
    fn claude_signals_are_switched_on_only_where_they_have_an_endpoint() {
        let traces_only = [
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://t:4318/v1/traces",
            ),
            ("OTEL_EXPORTER_OTLP_TRACES_HEADERS", "k=v"),
            ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/json"),
        ];
        let switches = |extra: &[(&str, &str)]| {
            let mut pairs = traces_only.to_vec();
            pairs.extend_from_slice(extra);
            let base = env(&pairs);
            let config = TelemetryConfig::from_env(&base).unwrap();
            let out = agent_env(
                Some(&config),
                &base,
                &identity(&AgentType::ClaudeCode, None),
                None,
            );
            [
                "CLAUDE_CODE_ENABLE_TELEMETRY",
                "OTEL_METRICS_EXPORTER",
                "OTEL_LOGS_EXPORTER",
                OTEL_EXPORTER_OTLP_PROTOCOL,
            ]
            .map(|key| get(&out, key).map(str::to_string))
        };
        let on = |v: &str| Some(v.to_string());
        assert_eq!(switches(&[]), [None, None, None, None]);
        assert_eq!(
            switches(&[("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "http://l:4318/v1/logs")]),
            [on("1"), None, on("otlp"), on("http/protobuf")]
        );
        assert_eq!(
            switches(&[(
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                "http://m:4318/v1/metrics"
            )]),
            [on("1"), on("otlp"), None, on("http/protobuf")]
        );
        assert_eq!(
            switches(&[(OTEL_EXPORTER_OTLP_ENDPOINT, "http://c:4318")]),
            [on("1"), on("otlp"), on("otlp"), on("http/protobuf")]
        );
        assert_eq!(
            switches(&[
                (OTEL_EXPORTER_OTLP_ENDPOINT, "http://c:4318"),
                (OTEL_EXPORTER_OTLP_PROTOCOL, "http/json"),
            ]),
            [on("1"), on("otlp"), on("otlp"), None],
            "the user's general protocol reaches Claude Code unchanged"
        );
    }

    #[test]
    fn other_agents_get_identity_only_and_unknowns_are_absent() {
        let out = agent_env(
            Some(&config(&[])),
            &EnvMap::new(),
            &identity(&AgentType::OpenCode, None),
            None,
        );
        assert_eq!(out.len(), 1);
        let attrs = parse_resource_attributes(&out[0].1);
        assert!(
            attrs
                .iter()
                .all(|(k, v)| AttrKey::is_allowed(k) && !v.is_empty())
        );
        assert!(
            !attrs
                .iter()
                .any(|(k, _)| k == "host.name" || k == "deck.role")
        );
        assert!(attrs.contains(&("deck.agent".into(), "opencode".into())));
    }

    #[test]
    fn a_grpc_configuration_still_propagates() {
        let out = agent_env(
            Some(&config(&[(OTEL_EXPORTER_OTLP_PROTOCOL, "grpc")])),
            &env(&[(OTEL_EXPORTER_OTLP_PROTOCOL, "grpc")]),
            &identity(&AgentType::ClaudeCode, None),
            None,
        );
        assert!(get(&out, OTEL_RESOURCE_ATTRIBUTES).is_some());
        assert_eq!(
            get(&out, OTEL_EXPORTER_OTLP_PROTOCOL),
            None,
            "the user set it"
        );
    }
}
