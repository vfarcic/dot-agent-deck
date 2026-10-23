//! Whether telemetry is on, and where it goes — read from an environment map
//! the caller supplies, so every rule here is testable without touching the
//! process environment. The daemon passes [`process_env`] once, at start.
//!
//! The rules follow the OpenTelemetry SDK environment-variable specification
//! for the subset the deck honours; an empty value counts as unset, as that
//! specification says it must.

use std::collections::HashMap;
use std::ffi::OsString;
use std::time::Duration;

use super::resource;

pub const OTEL_SDK_DISABLED: &str = "OTEL_SDK_DISABLED";
pub const OTEL_TRACES_EXPORTER: &str = "OTEL_TRACES_EXPORTER";
pub const OTEL_EXPORTER_OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
pub const OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT";
pub const OTEL_EXPORTER_OTLP_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
pub const OTEL_EXPORTER_OTLP_TRACES_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL";
pub const OTEL_EXPORTER_OTLP_HEADERS: &str = "OTEL_EXPORTER_OTLP_HEADERS";
pub const OTEL_EXPORTER_OTLP_TRACES_HEADERS: &str = "OTEL_EXPORTER_OTLP_TRACES_HEADERS";
pub const OTEL_EXPORTER_OTLP_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TIMEOUT";
pub const OTEL_EXPORTER_OTLP_TRACES_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT";
pub const OTEL_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
pub const OTEL_RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";

/// The OTLP export timeout when neither timeout variable is set, per the spec.
pub const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// A snapshot of an environment, keyed by variable name.
pub type EnvMap = HashMap<String, String>;

/// What [`process_env`] holds for one of [`SET_WHEN_UNREADABLE`] whose value
/// is not UTF-8: a value no reader takes for an `otlp` selector, an HTTP
/// protocol or `0`/`1`, so the variable is set, to something the deck cannot
/// read.
pub const UNREADABLE: &str = "\u{FFFD}";

/// The switches whose presence alone is the user's say: the ones the deck
/// sets for an agent only when the user has not, and the opt-outs and
/// selectors that turn an export off. One whose value is not UTF-8 is kept as
/// [`UNREADABLE`] rather than dropped, which would read as unset and have the
/// deck set it over the user's value.
pub const SET_WHEN_UNREADABLE: [&str; 10] = [
    OTEL_SDK_DISABLED,
    OTEL_TRACES_EXPORTER,
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
    OTEL_EXPORTER_OTLP_PROTOCOL,
    OTEL_EXPORTER_OTLP_TRACES_PROTOCOL,
    "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
    super::propagation::CLAUDE_CODE_ENABLE_TELEMETRY.0,
    super::launch::PI_TELEMETRY_ENV,
];

/// The daemon process's environment, for [`TelemetryConfig::from_env`].
/// A variable whose name is not UTF-8 is skipped, and so is one whose value
/// is not, but for [`SET_WHEN_UNREADABLE`].
pub fn process_env() -> EnvMap {
    env_from(std::env::vars_os())
}

/// [`process_env`] over `vars`.
pub(crate) fn env_from(vars: impl IntoIterator<Item = (OsString, OsString)>) -> EnvMap {
    vars.into_iter()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            let v = match v.into_string() {
                Ok(v) => v,
                Err(_) if SET_WHEN_UNREADABLE.contains(&k.as_str()) => UNREADABLE.to_string(),
                Err(_) => return None,
            };
            Some((k, v))
        })
        .collect()
}

/// Whether `env` switches the OTel SDK off: `OTEL_SDK_DISABLED` is `true`, in
/// any case, or set to a value the deck cannot read.
pub fn sdk_disabled(env: &EnvMap) -> bool {
    non_empty(env, OTEL_SDK_DISABLED)
        .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == UNREADABLE)
}

/// The value of `key` in `env`, or `None` when it is absent or empty after
/// trimming — the specification's "empty means unset".
pub fn non_empty<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
    env.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// The OTLP transport the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    /// `http/protobuf`, the specification's default.
    HttpProtobuf,
    /// `http/json`.
    HttpJson,
    /// `grpc`: recognised, not exported by the deck in this slice. Agents still
    /// receive the configuration, so their own telemetry still flows.
    Grpc,
    /// Anything else, as written. Treated like `grpc`: nothing exported.
    Unrecognised(String),
}

impl Protocol {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "http/protobuf" => Protocol::HttpProtobuf,
            "http/json" => Protocol::HttpJson,
            "grpc" => Protocol::Grpc,
            other => Protocol::Unrecognised(other.to_string()),
        }
    }

    /// The value as the `OTEL_EXPORTER_OTLP_PROTOCOL` variable spells it.
    pub fn as_env_value(&self) -> &str {
        match self {
            Protocol::HttpProtobuf => "http/protobuf",
            Protocol::HttpJson => "http/json",
            Protocol::Grpc => "grpc",
            Protocol::Unrecognised(v) => v,
        }
    }

    /// Whether the deck's own exporter can send with this protocol.
    pub fn deck_can_export(&self) -> bool {
        matches!(self, Protocol::HttpProtobuf | Protocol::HttpJson)
    }
}

/// The effective telemetry configuration. Existing at all means "on": spans are
/// exported when [`Protocol::deck_can_export`], and agents get propagation
/// either way.
///
/// `Debug` is written by hand so a header value can never reach a log line
/// through `{:?}`.
#[derive(Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// The traces endpoint, resolved per the specification.
    pub endpoint: String,
    pub protocol: Protocol,
    /// Export headers, percent-decoded. Values are secrets.
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub service_name: String,
    /// The user's `OTEL_RESOURCE_ATTRIBUTES` pairs, decoded, in order.
    pub user_resource_attributes: Vec<(String, String)>,
}

impl std::fmt::Debug for TelemetryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryConfig")
            .field("endpoint", &redacted_endpoint(&self.endpoint))
            .field("protocol", &self.protocol)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("timeout", &self.timeout)
            .field("service_name", &self.service_name)
            .field("user_resource_attributes", &self.user_resource_attributes)
            .finish()
    }
}

impl TelemetryConfig {
    /// The configuration `env` describes, or `None` when telemetry is off:
    /// `OTEL_SDK_DISABLED` is `true` (any case), `OTEL_TRACES_EXPORTER` is set
    /// and names no `otlp` exporter (`none`, `console`, …), or no endpoint is
    /// configured.
    pub fn from_env(env: &EnvMap) -> Option<Self> {
        if sdk_disabled(env) {
            return None;
        }
        if let Some(exporters) = non_empty(env, OTEL_TRACES_EXPORTER)
            && !exporters
                .split(',')
                .any(|e| e.trim().eq_ignore_ascii_case("otlp"))
        {
            return None;
        }
        let endpoint = resolve_endpoint(env)?;
        let protocol = non_empty(env, OTEL_EXPORTER_OTLP_TRACES_PROTOCOL)
            .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_PROTOCOL))
            .map(Protocol::parse)
            .unwrap_or(Protocol::HttpProtobuf);
        let timeout = non_empty(env, OTEL_EXPORTER_OTLP_TRACES_TIMEOUT)
            .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_TIMEOUT))
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_EXPORT_TIMEOUT);
        let service_name = non_empty(env, OTEL_SERVICE_NAME)
            .unwrap_or(super::DEFAULT_SERVICE_NAME)
            .to_string();
        let user_resource_attributes = non_empty(env, OTEL_RESOURCE_ATTRIBUTES)
            .map(resource::parse_resource_attributes)
            .unwrap_or_default();
        Some(TelemetryConfig {
            endpoint,
            protocol,
            headers: resolve_headers(env),
            timeout,
            service_name,
            user_resource_attributes,
        })
    }
}

/// The traces endpoint: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` as written, else
/// `OTEL_EXPORTER_OTLP_ENDPOINT` with `/v1/traces` appended (one slash between
/// them however the base ends). `None` when neither is set.
pub fn resolve_endpoint(env: &EnvMap) -> Option<String> {
    if let Some(traces) = non_empty(env, OTEL_EXPORTER_OTLP_TRACES_ENDPOINT) {
        return Some(traces.to_string());
    }
    non_empty(env, OTEL_EXPORTER_OTLP_ENDPOINT)
        .map(|base| format!("{}/v1/traces", base.trim_end_matches('/')))
}

/// The export headers: `OTEL_EXPORTER_OTLP_TRACES_HEADERS` when it is set,
/// else `OTEL_EXPORTER_OTLP_HEADERS`. The signal-specific variable replaces the
/// generic one as a whole, as the OTLP exporter specification has every
/// signal-specific option do: a generic header is never sent to a traces
/// destination that was given its own list. The list is `key=value` pairs
/// separated by commas, each part percent-decoded; an entry with no `=` or an
/// empty key is skipped, and a key named twice keeps its last value.
pub fn resolve_headers(env: &EnvMap) -> Vec<(String, String)> {
    let Some(raw) = non_empty(env, OTEL_EXPORTER_OTLP_TRACES_HEADERS)
        .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_HEADERS))
    else {
        return Vec::new();
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for (key, value) in resource::parse_key_value_list(raw) {
        if let Some(existing) = headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
        {
            existing.1 = value;
        } else {
            headers.push((key, value));
        }
    }
    headers
}

/// `endpoint` reduced to its scheme and host, for a log line: a URL's userinfo,
/// path or query can carry a credential, its host cannot.
pub fn redacted_endpoint(endpoint: &str) -> String {
    match reqwest::Url::parse(endpoint) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
            (Some(host), None) => format!("{}://{host}", url.scheme()),
            _ => format!("{}://", url.scheme()),
        },
        Err(_) => "<unparseable endpoint>".to_string(),
    }
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

    const BASE: (&str, &str) = (OTEL_EXPORTER_OTLP_ENDPOINT, "http://collector:4318");

    #[test]
    fn enablement_matrix() {
        let cases: &[(&[(&str, &str)], bool)] = &[
            (&[], false),
            (&[BASE], true),
            (&[(OTEL_EXPORTER_OTLP_ENDPOINT, "   ")], false),
            (
                &[(OTEL_EXPORTER_OTLP_TRACES_ENDPOINT, "http://c:4318/x")],
                true,
            ),
            (&[BASE, (OTEL_SDK_DISABLED, "true")], false),
            (&[BASE, (OTEL_SDK_DISABLED, "TRUE")], false),
            (&[BASE, (OTEL_SDK_DISABLED, "false")], true),
            (&[BASE, (OTEL_SDK_DISABLED, "")], true),
            (&[BASE, (OTEL_TRACES_EXPORTER, "none")], false),
            (&[BASE, (OTEL_TRACES_EXPORTER, "console")], false),
            (&[BASE, (OTEL_TRACES_EXPORTER, "otlp")], true),
            (&[BASE, (OTEL_TRACES_EXPORTER, "console, OTLP")], true),
            (&[BASE, (OTEL_TRACES_EXPORTER, "")], true),
            (&[(OTEL_SERVICE_NAME, "x")], false),
        ];
        for (pairs, on) in cases {
            assert_eq!(
                TelemetryConfig::from_env(&env(pairs)).is_some(),
                *on,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn endpoint_resolution() {
        assert_eq!(
            resolve_endpoint(&env(&[BASE])).as_deref(),
            Some("http://collector:4318/v1/traces")
        );
        assert_eq!(
            resolve_endpoint(&env(&[(OTEL_EXPORTER_OTLP_ENDPOINT, "https://api.x.io/")]))
                .as_deref(),
            Some("https://api.x.io/v1/traces")
        );
        assert_eq!(
            resolve_endpoint(&env(&[
                BASE,
                (
                    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT,
                    "https://t.example/custom"
                ),
            ]))
            .as_deref(),
            Some("https://t.example/custom"),
            "the signal-specific endpoint is used as written"
        );
    }

    #[test]
    fn protocol_resolution() {
        let protocol = |pairs: &[(&str, &str)]| {
            let mut all = vec![BASE];
            all.extend_from_slice(pairs);
            TelemetryConfig::from_env(&env(&all)).unwrap().protocol
        };
        assert_eq!(protocol(&[]), Protocol::HttpProtobuf);
        assert_eq!(
            protocol(&[(OTEL_EXPORTER_OTLP_PROTOCOL, "http/json")]),
            Protocol::HttpJson
        );
        assert_eq!(
            protocol(&[
                (OTEL_EXPORTER_OTLP_PROTOCOL, "http/json"),
                (OTEL_EXPORTER_OTLP_TRACES_PROTOCOL, "http/protobuf"),
            ]),
            Protocol::HttpProtobuf
        );
        assert_eq!(
            protocol(&[(OTEL_EXPORTER_OTLP_PROTOCOL, "grpc")]),
            Protocol::Grpc
        );
        assert!(!Protocol::Grpc.deck_can_export());
        assert!(!protocol(&[(OTEL_EXPORTER_OTLP_PROTOCOL, "carrier-pigeon")]).deck_can_export());
    }

    #[test]
    fn header_resolution_decodes_and_lets_traces_replace_the_generic_list() {
        let generic = (
            OTEL_EXPORTER_OTLP_HEADERS,
            "x-honeycomb-team=abc%3D%3D, x-other = 1,broken",
        );
        assert_eq!(
            resolve_headers(&env(&[generic])),
            vec![
                ("x-honeycomb-team".to_string(), "abc==".to_string()),
                ("x-other".to_string(), "1".to_string()),
            ]
        );
        assert_eq!(
            resolve_headers(&env(&[
                generic,
                (
                    OTEL_EXPORTER_OTLP_TRACES_HEADERS,
                    "X-Other=2,x-trace=%E2%9C%93,x-trace=3",
                ),
            ])),
            vec![
                ("X-Other".to_string(), "2".to_string()),
                ("x-trace".to_string(), "3".to_string()),
            ],
            "the traces list replaces the generic one: no generic header survives"
        );
        assert_eq!(
            resolve_headers(&env(&[generic, (OTEL_EXPORTER_OTLP_TRACES_HEADERS, " ")])).len(),
            2,
            "an empty traces list counts as unset"
        );
        assert!(resolve_headers(&env(&[])).is_empty());
    }

    #[test]
    fn service_name_timeout_and_resource_attributes() {
        let config = TelemetryConfig::from_env(&env(&[BASE])).unwrap();
        assert_eq!(config.service_name, "dot-agent-deck");
        assert_eq!(config.timeout, DEFAULT_EXPORT_TIMEOUT);
        let config = TelemetryConfig::from_env(&env(&[
            BASE,
            (OTEL_SERVICE_NAME, "my-deck"),
            (OTEL_EXPORTER_OTLP_TIMEOUT, "2500"),
            (OTEL_RESOURCE_ATTRIBUTES, "team=platform,env=dev%20box"),
        ]))
        .unwrap();
        assert_eq!(config.service_name, "my-deck");
        assert_eq!(config.timeout, Duration::from_millis(2500));
        assert_eq!(
            config.user_resource_attributes,
            vec![
                ("team".to_string(), "platform".to_string()),
                ("env".to_string(), "dev box".to_string()),
            ]
        );
    }

    #[test]
    fn debug_and_redaction_never_show_secrets() {
        let config = TelemetryConfig::from_env(&env(&[
            (
                OTEL_EXPORTER_OTLP_ENDPOINT,
                "https://user:pw-SENTINEL@api.example.io:4318/tok-SENTINEL?k=SENTINEL",
            ),
            (
                OTEL_EXPORTER_OTLP_HEADERS,
                "authorization=Bearer%20SENTINEL",
            ),
        ]))
        .unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains("SENTINEL"), "{shown}");
        assert!(shown.contains("authorization"));
        assert_eq!(
            redacted_endpoint(&config.endpoint),
            "https://api.example.io:4318"
        );
    }
}
