//! The collector's configuration, from the standard OpenTelemetry variables
//! the deck reads, with the metrics-specific variable of each taking the
//! place of the general one as the deck's traces variables do.

use std::time::Duration;

use crate::telemetry::config::{
    EnvMap, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_HEADERS, OTEL_EXPORTER_OTLP_PROTOCOL,
    OTEL_EXPORTER_OTLP_TIMEOUT, OTEL_RESOURCE_ATTRIBUTES, OTEL_SERVICE_NAME, Protocol, non_empty,
    sdk_disabled,
};
use crate::telemetry::resource::parse_key_value_list;

use super::SCOPE_NAME;
use super::identity::own_resource_pairs;

pub const OTEL_EXPORTER_OTLP_METRICS_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT";
pub const OTEL_EXPORTER_OTLP_METRICS_HEADERS: &str = "OTEL_EXPORTER_OTLP_METRICS_HEADERS";
pub const OTEL_EXPORTER_OTLP_METRICS_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL";
pub const OTEL_EXPORTER_OTLP_METRICS_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT";
pub const OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE: &str =
    "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE";
pub const OTEL_METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";

/// The export timeout when none is set.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(10_000);

/// How the CPU sum's points are aggregated over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporality {
    Delta,
    Cumulative,
}

impl Temporality {
    /// The OTLP `AggregationTemporality` value.
    pub fn otlp_value(self) -> u8 {
        match self {
            Temporality::Delta => 1,
            Temporality::Cumulative => 2,
        }
    }
}

/// What every resource the collector sends carries, and how its CPU is
/// aggregated: needed whether it sends or prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub service_name: String,
    /// The collector's own `OTEL_RESOURCE_ATTRIBUTES` pairs, filtered
    /// ([`own_resource_pairs`]).
    pub resource_pairs: Vec<(String, String)>,
    pub temporality: Temporality,
    /// The unrecognised temporality preference, for one warning.
    pub unknown_temporality: Option<String>,
}

impl Settings {
    pub fn from_env(env: &EnvMap) -> Self {
        let (temporality, unknown_temporality) =
            match non_empty(env, OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                None | Some("cumulative") => (Temporality::Cumulative, None),
                // `lowmemory` makes a counter delta; the CPU sum is the
                // collector's only one.
                Some("delta" | "lowmemory") => (Temporality::Delta, None),
                Some(other) => (Temporality::Cumulative, Some(other.to_string())),
            };
        Settings {
            service_name: non_empty(env, OTEL_SERVICE_NAME)
                .unwrap_or(SCOPE_NAME)
                .to_string(),
            resource_pairs: own_resource_pairs(non_empty(env, OTEL_RESOURCE_ATTRIBUTES)),
            temporality,
            unknown_temporality,
        }
    }
}

/// Where and how to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub protocol: Protocol,
    pub timeout: Duration,
}

/// Why the collector cannot send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// Neither the metrics endpoint nor the general one is set.
    NoEndpoint,
    /// `grpc` or a protocol nobody knows.
    UnsupportedProtocol(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::NoEndpoint => write!(
                f,
                "no OTLP endpoint: set {OTEL_EXPORTER_OTLP_ENDPOINT} or {OTEL_EXPORTER_OTLP_METRICS_ENDPOINT}, or pass --stdout"
            ),
            ExportError::UnsupportedProtocol(p) => write!(
                f,
                "unsupported OTLP protocol {p:?}: use http/protobuf or http/json"
            ),
        }
    }
}

impl Export {
    /// `Ok(None)` when export is switched off: `OTEL_SDK_DISABLED` is `true`,
    /// or `OTEL_METRICS_EXPORTER` is set and names no `otlp` exporter.
    pub fn from_env(env: &EnvMap) -> Result<Option<Self>, ExportError> {
        if sdk_disabled(env) {
            return Ok(None);
        }
        if let Some(exporters) = non_empty(env, OTEL_METRICS_EXPORTER)
            && !exporters
                .split(',')
                .any(|e| e.trim().eq_ignore_ascii_case("otlp"))
        {
            return Ok(None);
        }
        let endpoint = resolve_metrics_endpoint(env).ok_or(ExportError::NoEndpoint)?;
        let protocol = non_empty(env, OTEL_EXPORTER_OTLP_METRICS_PROTOCOL)
            .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_PROTOCOL))
            .map(Protocol::parse)
            .unwrap_or(Protocol::HttpProtobuf);
        if !protocol.deck_can_export() {
            return Err(ExportError::UnsupportedProtocol(
                protocol.as_env_value().to_string(),
            ));
        }
        let timeout = non_empty(env, OTEL_EXPORTER_OTLP_METRICS_TIMEOUT)
            .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_TIMEOUT))
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT);
        Ok(Some(Export {
            endpoint,
            headers: resolve_metrics_headers(env),
            protocol,
            timeout,
        }))
    }
}

/// `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` as written, else
/// `OTEL_EXPORTER_OTLP_ENDPOINT` with `/v1/metrics` appended (one slash
/// between them however the base ends).
pub fn resolve_metrics_endpoint(env: &EnvMap) -> Option<String> {
    if let Some(metrics) = non_empty(env, OTEL_EXPORTER_OTLP_METRICS_ENDPOINT) {
        return Some(metrics.to_string());
    }
    non_empty(env, OTEL_EXPORTER_OTLP_ENDPOINT)
        .map(|base| format!("{}/v1/metrics", base.trim_end_matches('/')))
}

/// `OTEL_EXPORTER_OTLP_METRICS_HEADERS` when set, else
/// `OTEL_EXPORTER_OTLP_HEADERS`; a key named twice (in any case) keeps its
/// last value.
pub fn resolve_metrics_headers(env: &EnvMap) -> Vec<(String, String)> {
    let Some(raw) = non_empty(env, OTEL_EXPORTER_OTLP_METRICS_HEADERS)
        .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_HEADERS))
    else {
        return Vec::new();
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for (key, value) in parse_key_value_list(raw) {
        match headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
        {
            Some(existing) => existing.1 = value,
            None => headers.push((key, value)),
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const BASE: (&str, &str) = (OTEL_EXPORTER_OTLP_ENDPOINT, "http://c:4318");

    #[test]
    fn endpoint_resolution() {
        for (pairs, expected) in [
            (vec![BASE], Some("http://c:4318/v1/metrics")),
            (
                vec![(OTEL_EXPORTER_OTLP_ENDPOINT, "http://c:4318/")],
                Some("http://c:4318/v1/metrics"),
            ),
            (
                vec![BASE, (OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, "http://m:1/x")],
                Some("http://m:1/x"),
            ),
            (vec![(OTEL_EXPORTER_OTLP_ENDPOINT, "  ")], None),
            (
                vec![("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://t:1")],
                None,
            ),
        ] {
            assert_eq!(
                resolve_metrics_endpoint(&env(&pairs)).as_deref(),
                expected,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn export_switches() {
        assert_eq!(Export::from_env(&env(&[])), Err(ExportError::NoEndpoint));
        assert_eq!(
            Export::from_env(&env(&[BASE, ("OTEL_SDK_DISABLED", "TRUE")])),
            Ok(None)
        );
        assert_eq!(
            Export::from_env(&env(&[("OTEL_SDK_DISABLED", "true")])),
            Ok(None),
            "disabled wins over a missing endpoint"
        );
        assert_eq!(
            Export::from_env(&env(&[BASE, (OTEL_METRICS_EXPORTER, "none")])),
            Ok(None)
        );
        assert!(matches!(
            Export::from_env(&env(&[BASE, (OTEL_METRICS_EXPORTER, "console, OTLP")])),
            Ok(Some(_))
        ));
    }

    #[test]
    fn protocol_and_timeout() {
        let export = Export::from_env(&env(&[BASE])).unwrap().unwrap();
        assert_eq!(export.protocol, Protocol::HttpProtobuf);
        assert_eq!(export.timeout, DEFAULT_TIMEOUT);
        let export = Export::from_env(&env(&[
            BASE,
            (OTEL_EXPORTER_OTLP_PROTOCOL, "http/protobuf"),
            (OTEL_EXPORTER_OTLP_METRICS_PROTOCOL, "HTTP/JSON"),
            (OTEL_EXPORTER_OTLP_TIMEOUT, "5000"),
            (OTEL_EXPORTER_OTLP_METRICS_TIMEOUT, "250"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(export.protocol, Protocol::HttpJson);
        assert_eq!(export.timeout, Duration::from_millis(250));
        assert_eq!(
            Export::from_env(&env(&[BASE, (OTEL_EXPORTER_OTLP_PROTOCOL, "grpc")])),
            Err(ExportError::UnsupportedProtocol("grpc".into()))
        );
        assert_eq!(
            Export::from_env(&env(&[
                BASE,
                (OTEL_EXPORTER_OTLP_PROTOCOL, "carrier-pigeon")
            ])),
            Err(ExportError::UnsupportedProtocol("carrier-pigeon".into()))
        );
    }

    #[test]
    fn the_metrics_headers_replace_the_general_ones() {
        assert_eq!(
            resolve_metrics_headers(&env(&[(
                OTEL_EXPORTER_OTLP_HEADERS,
                "x-key=a%20b,X-Key=c,team=t"
            )])),
            vec![("x-key".into(), "c".into()), ("team".into(), "t".into())]
        );
        assert_eq!(
            resolve_metrics_headers(&env(&[
                (OTEL_EXPORTER_OTLP_HEADERS, "general=1"),
                (OTEL_EXPORTER_OTLP_METRICS_HEADERS, "metrics=2"),
            ])),
            vec![("metrics".into(), "2".into())]
        );
        assert!(resolve_metrics_headers(&env(&[])).is_empty());
    }

    #[test]
    fn settings() {
        let s = Settings::from_env(&env(&[]));
        assert_eq!(s.service_name, "dot-agent-deck-collector");
        assert_eq!(s.temporality, Temporality::Cumulative);
        assert!(s.resource_pairs.is_empty());
        let s = Settings::from_env(&env(&[
            (OTEL_SERVICE_NAME, "mine"),
            (OTEL_RESOURCE_ATTRIBUTES, "deck.run.id=forged,team=x"),
            (OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE, "Delta"),
        ]));
        assert_eq!(s.service_name, "mine");
        assert_eq!(s.resource_pairs, vec![("team".into(), "x".into())]);
        assert_eq!(s.temporality, Temporality::Delta);
        for (value, temporality, unknown) in [
            ("lowmemory", Temporality::Delta, None),
            ("CUMULATIVE", Temporality::Cumulative, None),
            ("sometimes", Temporality::Cumulative, Some("sometimes")),
        ] {
            let s = Settings::from_env(&env(&[(
                OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE,
                value,
            )]));
            assert_eq!(s.temporality, temporality, "{value}");
            assert_eq!(s.unknown_temporality.as_deref(), unknown, "{value}");
        }
        assert_eq!(Temporality::Delta.otlp_value(), 1);
        assert_eq!(Temporality::Cumulative.otlp_value(), 2);
    }
}
