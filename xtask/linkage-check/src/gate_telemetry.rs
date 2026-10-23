//! Issue #1850: the OpenTelemetry spans `cargo xtask affected-checks --run`
//! sends — one `affected_checks.run` per invocation and one
//! `affected_checks.command` child per command it runs, saying where each
//! command ran (`local`, or the runner `DAD_REMOTE_EXEC` names) and how long it
//! took. `docs/develop/gate-telemetry.md` lists the attributes.
//!
//! **Off unless the standard `OTEL_*` variables turn it on.** With no
//! `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, or with
//! `OTEL_SDK_DISABLED=true`, [`config_from`] returns `None`, no exporter is
//! built and nothing reaches the network. This module writes no output of its
//! own, so the helper prints the same lines with telemetry on or off.
//!
//! **It never fails or stalls the gates.** The send runs on its own thread and
//! [`report`] waits for it at most [`EXPORT_BOUND`]; an error or a send still
//! running after that is dropped, and the exit status is the gates' own.
//!
//! The workspace has no OpenTelemetry crate to reuse: the deck's own export
//! (PRD #1741, `src/telemetry/otlp.rs`) encodes OTLP by hand over `reqwest`,
//! and this does the same for the few fields it fills, with the same
//! `reqwest`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const SPAN_RUN: &str = "affected_checks.run";
pub(crate) const SPAN_COMMAND: &str = "affected_checks.command";

/// The longest [`report`] waits for a send, whatever `OTEL_EXPORTER_OTLP_TIMEOUT`
/// says: an unreachable collector costs a gate run at most this.
pub(crate) const EXPORT_BOUND: Duration = Duration::from_secs(3);

/// `service.name` when neither `OTEL_SERVICE_NAME` nor a `service.name` pair in
/// `OTEL_RESOURCE_ATTRIBUTES` sets one: the deck's own, so the spans land
/// beside the deck's `agent.run` and `agent.operation` spans.
pub(crate) const DEFAULT_SERVICE_NAME: &str = "dot-agent-deck";

const SCOPE_NAME: &str = "xtask-affected-checks";

/// `SpanKind.SPAN_KIND_INTERNAL`.
const SPAN_KIND_INTERNAL: u64 = 1;
/// `Status.StatusCode.STATUS_CODE_ERROR`.
const STATUS_CODE_ERROR: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    HttpProtobuf,
    HttpJson,
}

/// Where and how to send, read from the standard `OTEL_*` variables.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Config {
    pub(crate) endpoint: String,
    pub(crate) protocol: Protocol,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) timeout: Duration,
    /// `service.name` first, then every `OTEL_RESOURCE_ATTRIBUTES` pair.
    pub(crate) resource: Vec<(String, String)>,
}

/// Header values are credentials, so only their names are printed.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("endpoint", &self.endpoint)
            .field("protocol", &self.protocol)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("timeout", &self.timeout)
            .field("resource", &self.resource)
            .finish()
    }
}

/// The export configuration, or `None` when the environment does not turn
/// export on. `env` looks a variable up; [`from_process_env`] passes the real
/// environment.
///
/// Off when `OTEL_SDK_DISABLED` is `true`, when `OTEL_TRACES_EXPORTER` is set
/// and names no `otlp`, when no endpoint is set, and when the protocol is one
/// this cannot send (`grpc`, or anything unrecognised): a `grpc` endpoint does
/// not answer an HTTP POST, so sending would only spend the bound.
pub(crate) fn config_from(env: &dyn Fn(&str) -> Option<String>) -> Option<Config> {
    let get = |key: &str| {
        env(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if get("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return None;
    }
    if let Some(exporters) = get("OTEL_TRACES_EXPORTER")
        && !exporters
            .split(',')
            .any(|e| e.trim().eq_ignore_ascii_case("otlp"))
    {
        return None;
    }
    let endpoint = match get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
        Some(traces) => traces,
        None => format!(
            "{}/v1/traces",
            get("OTEL_EXPORTER_OTLP_ENDPOINT")?.trim_end_matches('/')
        ),
    };
    let protocol = match get("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
        .or_else(|| get("OTEL_EXPORTER_OTLP_PROTOCOL"))
        .map(|p| p.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("http/protobuf") => Protocol::HttpProtobuf,
        Some("http/json") => Protocol::HttpJson,
        Some(_) => return None,
    };
    let headers = get("OTEL_EXPORTER_OTLP_TRACES_HEADERS")
        .or_else(|| get("OTEL_EXPORTER_OTLP_HEADERS"))
        .map(|raw| key_value_list(&raw))
        .unwrap_or_default();
    let timeout = get("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT")
        .or_else(|| get("OTEL_EXPORTER_OTLP_TIMEOUT"))
        .and_then(|ms| ms.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .map_or(EXPORT_BOUND, |t| t.min(EXPORT_BOUND));
    // As an SDK resolves it: `OTEL_SERVICE_NAME` wins over a `service.name`
    // pair, and a later pair wins over an earlier one with the same key.
    let mut user: Vec<(String, String)> = Vec::new();
    for (key, value) in get("OTEL_RESOURCE_ATTRIBUTES")
        .map(|raw| key_value_list(&raw))
        .unwrap_or_default()
    {
        user.retain(|(k, _)| *k != key);
        user.push((key, value));
    }
    let service = get("OTEL_SERVICE_NAME")
        .or_else(|| {
            user.iter()
                .find(|(k, _)| k == "service.name")
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());
    let mut resource = vec![("service.name".to_string(), service)];
    resource.extend(user.into_iter().filter(|(k, _)| k != "service.name"));
    Some(Config {
        endpoint,
        protocol,
        headers,
        timeout,
        resource,
    })
}

pub(crate) fn from_process_env() -> Option<Config> {
    config_from(&|key| std::env::var(key).ok())
}

/// The W3C Baggage `key=value,key2=value2` list `OTEL_RESOURCE_ATTRIBUTES` and
/// `OTEL_EXPORTER_OTLP_HEADERS` use, values percent-decoded.
fn key_value_list(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            let key = percent_decode(key.trim());
            (!key.is_empty()).then(|| (key, percent_decode(value.trim())))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Str(String),
    Int(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) trace_id: [u8; 16],
    pub(crate) span_id: [u8; 8],
    pub(crate) parent_span_id: Option<[u8; 8]>,
    pub(crate) name: &'static str,
    pub(crate) start_unix_nanos: u64,
    pub(crate) end_unix_nanos: u64,
    pub(crate) attributes: Vec<(&'static str, Value)>,
    pub(crate) error: bool,
}

impl Span {
    #[cfg(test)]
    pub(crate) fn attr(&self, key: &str) -> Option<&Value> {
        self.attributes
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }
}

/// What one export sends: the resource and the run's spans, the run first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Batch {
    pub(crate) resource: Vec<(String, String)>,
    pub(crate) spans: Vec<Span>,
}

/// What the run span says about the runner and the plan, known before the
/// first command starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunInfo {
    /// What `DAD_REMOTE_EXEC` asked for, `local` when it is unset or empty.
    pub(crate) requested: String,
    /// `local`, or the runner's name.
    pub(crate) used: &'static str,
    /// Why a requested runner was not used, as reason codes joined by `,`.
    pub(crate) fallback_reason: Option<String>,
    /// `full`, `text` or `none` (no change, so no command).
    pub(crate) plan: &'static str,
    pub(crate) commands: usize,
}

/// One command's result: its exit code when it exited with one, and whether
/// it passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandResult {
    pub(crate) exit_code: Option<i32>,
    pub(crate) passed: bool,
}

/// Records a run's spans while it runs. Recording is a few vector pushes, so it
/// happens whether or not anything will be sent.
pub(crate) struct RunTrace {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    start_unix_nanos: u64,
    started: Instant,
    info: RunInfo,
    commands: Vec<Span>,
    failed_command: Option<String>,
}

fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// `N` random bytes, never all zero (an all-zero id is invalid in OTLP).
/// `RandomState` is seeded from the operating system's randomness, so this
/// needs no crate the workspace does not already build.
fn random_id<const N: usize>() -> [u8; N] {
    loop {
        let mut out = [0u8; N];
        for chunk in out.chunks_mut(8) {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(unix_nanos_now());
            hasher.write_u32(std::process::id());
            chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
        }
        if out.iter().any(|b| *b != 0) {
            return out;
        }
    }
}

/// When a run began: the run span starts here, before the runner is probed,
/// so a remote runner's probe counts towards the run's time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Began {
    unix_nanos: u64,
    at: Instant,
}

impl Began {
    pub(crate) fn now() -> Self {
        Began {
            unix_nanos: unix_nanos_now(),
            at: Instant::now(),
        }
    }
}

impl RunTrace {
    pub(crate) fn start(began: Began, info: RunInfo) -> Self {
        RunTrace {
            trace_id: random_id(),
            span_id: random_id(),
            start_unix_nanos: began.unix_nanos,
            started: began.at,
            info,
            commands: Vec::new(),
            failed_command: None,
        }
    }

    /// Records one command that started at `started` and has just finished.
    /// `command` is the command as planned, without a runner's prefix.
    pub(crate) fn command(
        &mut self,
        command: String,
        runner: &'static str,
        started: Instant,
        result: CommandResult,
    ) {
        let end = Instant::now();
        let offset = |at: Instant| {
            self.start_unix_nanos + at.saturating_duration_since(self.started).as_nanos() as u64
        };
        let mut attributes = vec![
            ("gates.command", Value::Str(command.clone())),
            ("gates.command.runner", Value::Str(runner.to_string())),
            (
                "gates.command.outcome",
                Value::Str(outcome(result.passed).to_string()),
            ),
        ];
        if let Some(code) = result.exit_code {
            attributes.push(("gates.command.exit_code", Value::Int(i64::from(code))));
        }
        self.commands.push(Span {
            trace_id: self.trace_id,
            span_id: random_id(),
            parent_span_id: Some(self.span_id),
            name: SPAN_COMMAND,
            start_unix_nanos: offset(started),
            end_unix_nanos: offset(end),
            attributes,
            error: !result.passed,
        });
        if !result.passed && self.failed_command.is_none() {
            self.failed_command = Some(command);
        }
    }

    /// Ends the run span now and returns every span, the run first.
    pub(crate) fn finish(self) -> Vec<Span> {
        let end = self.start_unix_nanos + self.started.elapsed().as_nanos() as u64;
        let info = self.info;
        let mut attributes = vec![
            ("gates.runner.requested", Value::Str(info.requested)),
            ("gates.runner.used", Value::Str(info.used.to_string())),
        ];
        if let Some(reason) = info.fallback_reason {
            attributes.push(("gates.runner.fallback_reason", Value::Str(reason)));
        }
        attributes.push(("gates.plan", Value::Str(info.plan.to_string())));
        attributes.push(("gates.commands.count", Value::Int(info.commands as i64)));
        attributes.push((
            "gates.outcome",
            Value::Str(outcome(self.failed_command.is_none()).to_string()),
        ));
        let error = self.failed_command.is_some();
        if let Some(failed) = self.failed_command {
            attributes.push(("gates.failed_command", Value::Str(failed)));
        }
        let mut spans = vec![Span {
            trace_id: self.trace_id,
            span_id: self.span_id,
            parent_span_id: None,
            name: SPAN_RUN,
            start_unix_nanos: self.start_unix_nanos,
            end_unix_nanos: end,
            attributes,
            error,
        }];
        spans.extend(self.commands);
        spans
    }
}

fn outcome(passed: bool) -> &'static str {
    if passed { "passed" } else { "failed" }
}

/// Sends a batch somewhere. The real one is [`Http`]; the tests use in-memory
/// ones.
pub(crate) trait Exporter: Send + 'static {
    fn export(&self, batch: &Batch) -> Result<(), String>;
}

/// OTLP over HTTP, one attempt, no redirects followed.
pub(crate) struct Http(pub(crate) Config);

impl Exporter for Http {
    fn export(&self, batch: &Batch) -> Result<(), String> {
        let config = &self.0;
        let (body, content_type) = match config.protocol {
            Protocol::HttpProtobuf => (encode_protobuf(batch), "application/x-protobuf"),
            Protocol::HttpJson => (encode_json(batch), "application/json"),
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let mut request = client
            .post(&config.endpoint)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body);
        for (key, value) in &config.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        let response = request.send().map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("HTTP {}", response.status()))
        }
    }
}

/// Sends `spans` through `exporter` and returns `status` unchanged.
///
/// The send runs on its own thread, and this waits for it at most `bound`. Its
/// error, its panic and a send still running after `bound` are all dropped
/// without a word: a thread still sending when `main` returns ends with the
/// process.
pub(crate) fn report(
    status: ExitCode,
    exporter: Option<Box<dyn Exporter>>,
    resource: Vec<(String, String)>,
    spans: Vec<Span>,
    bound: Duration,
) -> ExitCode {
    let Some(exporter) = exporter else {
        return status;
    };
    let batch = Batch { resource, spans };
    let (done, wait) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("gate-telemetry".to_string())
        .spawn(move || {
            let _ = done.send(exporter.export(&batch));
        });
    if spawned.is_ok() {
        let _ = wait.recv_timeout(bound);
    }
    status
}

// ---- OTLP encoding: only the fields filled above ----

fn put_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(buf, (u64::from(field) << 3) | u64::from(wire));
}

fn put_bytes(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_tag(buf, field, 2);
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

fn put_uint(buf: &mut Vec<u8>, field: u32, value: u64) {
    put_tag(buf, field, 0);
    put_varint(buf, value);
}

fn put_fixed64(buf: &mut Vec<u8>, field: u32, value: u64) {
    put_tag(buf, field, 1);
    buf.extend_from_slice(&value.to_le_bytes());
}

/// `common.v1.KeyValue`, with an `AnyValue` of `string_value` (1) or
/// `int_value` (3).
fn key_value(key: &str, value: &Value) -> Vec<u8> {
    let mut any = Vec::new();
    match value {
        Value::Str(s) => put_bytes(&mut any, 1, s.as_bytes()),
        Value::Int(i) => put_uint(&mut any, 3, *i as u64),
    }
    let mut buf = Vec::new();
    put_bytes(&mut buf, 1, key.as_bytes());
    put_bytes(&mut buf, 2, &any);
    buf
}

/// An `ExportTraceServiceRequest` for `http/protobuf`.
pub(crate) fn encode_protobuf(batch: &Batch) -> Vec<u8> {
    let mut resource = Vec::new();
    for (key, value) in &batch.resource {
        put_bytes(
            &mut resource,
            1,
            &key_value(key, &Value::Str(value.clone())),
        );
    }
    let mut scope = Vec::new();
    put_bytes(&mut scope, 1, SCOPE_NAME.as_bytes());
    let mut scope_spans = Vec::new();
    put_bytes(&mut scope_spans, 1, &scope);
    for span in &batch.spans {
        let mut msg = Vec::new();
        put_bytes(&mut msg, 1, &span.trace_id);
        put_bytes(&mut msg, 2, &span.span_id);
        if let Some(parent) = &span.parent_span_id {
            put_bytes(&mut msg, 4, parent);
        }
        put_bytes(&mut msg, 5, span.name.as_bytes());
        put_uint(&mut msg, 6, SPAN_KIND_INTERNAL);
        put_fixed64(&mut msg, 7, span.start_unix_nanos);
        put_fixed64(&mut msg, 8, span.end_unix_nanos);
        for (key, value) in &span.attributes {
            put_bytes(&mut msg, 9, &key_value(key, value));
        }
        if span.error {
            let mut status = Vec::new();
            put_uint(&mut status, 3, STATUS_CODE_ERROR);
            put_bytes(&mut msg, 15, &status);
        }
        put_bytes(&mut scope_spans, 2, &msg);
    }
    let mut resource_spans = Vec::new();
    put_bytes(&mut resource_spans, 1, &resource);
    put_bytes(&mut resource_spans, 2, &scope_spans);
    let mut request = Vec::new();
    put_bytes(&mut request, 1, &resource_spans);
    request
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn json_kv(key: &str, value: &Value) -> serde_json::Value {
    let value = match value {
        Value::Str(s) => serde_json::json!({ "stringValue": s }),
        // OTLP/JSON writes 64-bit integers as decimal strings.
        Value::Int(i) => serde_json::json!({ "intValue": i.to_string() }),
    };
    serde_json::json!({ "key": key, "value": value })
}

/// The OTLP/JSON body for `http/json`; ids are lowercase hex, as that mapping
/// requires.
pub(crate) fn encode_json(batch: &Batch) -> Vec<u8> {
    let spans: Vec<serde_json::Value> = batch
        .spans
        .iter()
        .map(|span| {
            let mut value = serde_json::json!({
                "traceId": hex(&span.trace_id),
                "spanId": hex(&span.span_id),
                "name": span.name,
                "kind": SPAN_KIND_INTERNAL,
                "startTimeUnixNano": span.start_unix_nanos.to_string(),
                "endTimeUnixNano": span.end_unix_nanos.to_string(),
                "attributes": span.attributes.iter()
                    .map(|(k, v)| json_kv(k, v))
                    .collect::<Vec<_>>(),
            });
            if let Some(parent) = &span.parent_span_id {
                value["parentSpanId"] = serde_json::Value::String(hex(parent));
            }
            if span.error {
                value["status"] = serde_json::json!({ "code": STATUS_CODE_ERROR });
            }
            value
        })
        .collect();
    serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": batch.resource.iter()
                    .map(|(k, v)| json_kv(k, &Value::Str(v.clone())))
                    .collect::<Vec<_>>(),
            },
            "scopeSpans": [{
                "scope": { "name": SCOPE_NAME },
                "spans": spans,
            }],
        }],
    })
    .to_string()
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    const BASE: (&str, &str) = ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector:4318/");

    #[test]
    fn no_otel_environment_builds_no_exporter() {
        let off: &[&[(&str, &str)]] = &[
            &[],
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "  ")],
            &[
                ("OTEL_SERVICE_NAME", "x"),
                ("OTEL_RESOURCE_ATTRIBUTES", "a=b"),
            ],
            &[BASE, ("OTEL_SDK_DISABLED", "true")],
            &[BASE, ("OTEL_SDK_DISABLED", "TRUE")],
            &[BASE, ("OTEL_TRACES_EXPORTER", "none")],
            &[BASE, ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")],
            &[BASE, ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc")],
        ];
        for pairs in off {
            assert_eq!(config_from(&env(pairs)), None, "{pairs:?}");
        }
        let on: &[&[(&str, &str)]] = &[
            &[BASE],
            &[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://c:4318/x")],
            &[BASE, ("OTEL_SDK_DISABLED", "false")],
            &[BASE, ("OTEL_TRACES_EXPORTER", "console, OTLP")],
            &[BASE, ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")],
        ];
        for pairs in on {
            assert!(config_from(&env(pairs)).is_some(), "{pairs:?}");
        }
    }

    #[test]
    fn the_endpoint_protocol_headers_and_timeout_follow_the_standard_variables() {
        let config = config_from(&env(&[
            BASE,
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "x-honeycomb-team=k%3D1, other=2",
            ),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "500"),
        ]))
        .unwrap();
        assert_eq!(config.endpoint, "https://collector:4318/v1/traces");
        assert_eq!(config.protocol, Protocol::HttpProtobuf);
        assert_eq!(
            config.headers,
            [
                ("x-honeycomb-team".to_string(), "k=1".to_string()),
                ("other".to_string(), "2".to_string()),
            ]
        );
        assert_eq!(config.timeout, Duration::from_millis(500));
        assert!(
            !format!("{config:?}").contains("k=1"),
            "header values are not printed"
        );

        let config = config_from(&env(&[
            BASE,
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://c:4318/x"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "a=1"),
            ("OTEL_EXPORTER_OTLP_TRACES_HEADERS", "b=2"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
            ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "HTTP/JSON"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "600000"),
        ]))
        .unwrap();
        assert_eq!(config.endpoint, "http://c:4318/x");
        assert_eq!(config.protocol, Protocol::HttpJson);
        assert_eq!(config.headers, [("b".to_string(), "2".to_string())]);
        assert_eq!(config.timeout, EXPORT_BOUND, "a long timeout is capped");
    }

    #[test]
    fn otel_resource_attributes_reach_the_resource() {
        let deck = "deck.run.id=run-1,deck.pane.id=7,deck.role=coder,host.name=box";
        let config = config_from(&env(&[BASE, ("OTEL_RESOURCE_ATTRIBUTES", deck)])).unwrap();
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            config.resource,
            pairs(&[
                ("service.name", DEFAULT_SERVICE_NAME),
                ("deck.run.id", "run-1"),
                ("deck.pane.id", "7"),
                ("deck.role", "coder"),
                ("host.name", "box"),
            ])
        );

        let batch = Batch {
            resource: config.resource.clone(),
            spans: RunTrace::start(Began::now(), info()).finish(),
        };
        let json: serde_json::Value = serde_json::from_slice(&encode_json(&batch)).unwrap();
        let attrs = &json["resourceSpans"][0]["resource"]["attributes"];
        for (key, value) in &config.resource {
            assert!(
                attrs.as_array().unwrap().iter().any(
                    |a| a["key"] == key.as_str() && a["value"]["stringValue"] == value.as_str()
                ),
                "{key}={value} missing from {attrs}"
            );
        }
        let protobuf = encode_protobuf(&batch);
        for (key, value) in &config.resource {
            for needle in [key, value] {
                assert!(
                    protobuf
                        .windows(needle.len())
                        .any(|w| w == needle.as_bytes()),
                    "{needle} missing from the protobuf body"
                );
            }
        }
    }

    #[test]
    fn the_service_name_resolves_as_an_sdk_resolves_it() {
        let name = |pairs: &[(&str, &str)]| config_from(&env(pairs)).unwrap().resource;
        assert_eq!(
            name(&[BASE, ("OTEL_RESOURCE_ATTRIBUTES", "service.name=a,k=v")]),
            [
                ("service.name".to_string(), "a".to_string()),
                ("k".to_string(), "v".to_string())
            ]
        );
        assert_eq!(
            name(&[
                BASE,
                ("OTEL_RESOURCE_ATTRIBUTES", "service.name=a"),
                ("OTEL_SERVICE_NAME", "b"),
            ]),
            [("service.name".to_string(), "b".to_string())]
        );
    }

    fn info() -> RunInfo {
        RunInfo {
            requested: "rwx".to_string(),
            used: "rwx",
            fallback_reason: None,
            plan: "full",
            commands: 3,
        }
    }

    #[test]
    fn each_command_is_a_child_of_the_run_and_the_first_failure_is_the_outcome() {
        let mut trace = RunTrace::start(Began::now(), info());
        let passed = CommandResult {
            exit_code: Some(0),
            passed: true,
        };
        let failed = CommandResult {
            exit_code: Some(101),
            passed: false,
        };
        trace.command("cargo fmt --check".into(), "local", Instant::now(), passed);
        trace.command("cargo test-fast".into(), "rwx", Instant::now(), failed);
        let spans = trace.finish();
        let [run, fmt, tests] = spans.as_slice() else {
            panic!("{spans:?}")
        };
        assert_eq!(run.name, SPAN_RUN);
        assert_eq!(run.parent_span_id, None);
        assert!(run.error);
        assert_eq!(
            run.attr("gates.outcome"),
            Some(&Value::Str("failed".into()))
        );
        assert_eq!(
            run.attr("gates.failed_command"),
            Some(&Value::Str("cargo test-fast".into()))
        );
        assert_eq!(run.attr("gates.commands.count"), Some(&Value::Int(3)));
        for (child, command, runner, code) in [
            (fmt, "cargo fmt --check", "local", 0),
            (tests, "cargo test-fast", "rwx", 101),
        ] {
            assert_eq!(child.name, SPAN_COMMAND);
            assert_eq!(child.trace_id, run.trace_id);
            assert_eq!(child.parent_span_id, Some(run.span_id));
            assert_ne!(child.span_id, run.span_id);
            assert_eq!(
                child.attr("gates.command"),
                Some(&Value::Str(command.into()))
            );
            assert_eq!(
                child.attr("gates.command.runner"),
                Some(&Value::Str(runner.into()))
            );
            assert_eq!(
                child.attr("gates.command.exit_code"),
                Some(&Value::Int(code))
            );
            assert!(run.start_unix_nanos <= child.start_unix_nanos);
            assert!(child.start_unix_nanos <= child.end_unix_nanos);
            assert!(child.end_unix_nanos <= run.end_unix_nanos);
        }
        assert!(!fmt.error && tests.error);
    }

    #[test]
    fn a_passing_run_has_no_failed_command_and_no_error_status() {
        let mut trace = RunTrace::start(Began::now(), info());
        let result = CommandResult {
            exit_code: None,
            passed: true,
        };
        trace.command("true".into(), "local", Instant::now(), result);
        let spans = trace.finish();
        assert_eq!(
            spans[0].attr("gates.outcome"),
            Some(&Value::Str("passed".into()))
        );
        assert_eq!(spans[0].attr("gates.failed_command"), None);
        assert_eq!(spans[1].attr("gates.command.exit_code"), None);
        assert!(spans.iter().all(|s| !s.error));
        let json = String::from_utf8(encode_json(&Batch {
            resource: Vec::new(),
            spans,
        }))
        .unwrap();
        assert!(!json.contains("status"), "{json}");
    }

    struct Failing;
    impl Exporter for Failing {
        fn export(&self, _: &Batch) -> Result<(), String> {
            Err("connection refused".to_string())
        }
    }

    struct Panicking;
    impl Exporter for Panicking {
        fn export(&self, _: &Batch) -> Result<(), String> {
            panic!("exporter bug")
        }
    }

    /// Never returns: it waits on a channel whose sender the test keeps.
    struct Hanging(Mutex<mpsc::Receiver<()>>);
    impl Exporter for Hanging {
        fn export(&self, _: &Batch) -> Result<(), String> {
            let _ = self.0.lock().unwrap().recv();
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct Memory(Arc<Mutex<Vec<Batch>>>);
    impl Exporter for Memory {
        fn export(&self, batch: &Batch) -> Result<(), String> {
            self.0.lock().unwrap().push(batch.clone());
            Ok(())
        }
    }

    #[test]
    fn an_export_failure_does_not_change_the_exit_code() {
        for status in [ExitCode::SUCCESS, ExitCode::FAILURE] {
            let spans = || RunTrace::start(Began::now(), info()).finish();
            let bound = Duration::from_secs(5);
            assert_eq!(
                report(status, Some(Box::new(Failing)), Vec::new(), spans(), bound),
                status
            );
            assert_eq!(
                report(
                    status,
                    Some(Box::new(Panicking)),
                    Vec::new(),
                    spans(),
                    bound
                ),
                status
            );
            assert_eq!(report(status, None, Vec::new(), spans(), bound), status);
        }
    }

    #[test]
    fn a_send_that_does_not_finish_is_abandoned_at_the_bound() {
        let (_keep, never) = mpsc::channel();
        let started = Instant::now();
        let status = report(
            ExitCode::FAILURE,
            Some(Box::new(Hanging(Mutex::new(never)))),
            Vec::new(),
            RunTrace::start(Began::now(), info()).finish(),
            Duration::from_millis(50),
        );
        assert_eq!(status, ExitCode::FAILURE);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_exporter_receives_the_resource_and_every_span() {
        let memory = Memory::default();
        let resource = vec![("service.name".to_string(), "s".to_string())];
        let spans = RunTrace::start(Began::now(), info()).finish();
        report(
            ExitCode::SUCCESS,
            Some(Box::new(memory.clone())),
            resource.clone(),
            spans.clone(),
            Duration::from_secs(5),
        );
        assert_eq!(*memory.0.lock().unwrap(), [Batch { resource, spans }]);
    }

    #[test]
    fn the_json_body_is_otlp_json() {
        let mut trace = RunTrace::start(Began::now(), info());
        trace.command(
            "cargo test-fast".into(),
            "rwx",
            Instant::now(),
            CommandResult {
                exit_code: Some(1),
                passed: false,
            },
        );
        let spans = trace.finish();
        let json: serde_json::Value = serde_json::from_slice(&encode_json(&Batch {
            resource: Vec::new(),
            spans: spans.clone(),
        }))
        .unwrap();
        let out = &json["resourceSpans"][0]["scopeSpans"][0]["spans"];
        assert_eq!(out[0]["name"], SPAN_RUN);
        assert_eq!(out[0]["traceId"], hex(&spans[0].trace_id));
        assert_eq!(out[0]["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(out[1]["parentSpanId"], hex(&spans[0].span_id));
        assert_eq!(out[1]["status"]["code"], STATUS_CODE_ERROR);
        assert!(
            out[1]["attributes"].as_array().unwrap().iter().any(|a| {
                a["key"] == "gates.command.exit_code" && a["value"]["intValue"] == "1"
            })
        );
    }

    #[test]
    fn ids_are_random_and_never_zero() {
        let a: [u8; 16] = random_id();
        let b: [u8; 16] = random_id();
        assert_ne!(a, b);
        assert!(a.iter().any(|x| *x != 0));
    }
}
