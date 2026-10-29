//! Issue #1981 M8: the daemon's own warnings and errors as OTLP logs, so a
//! problem the daemon writes to `deck.log` is visible next to the runs.
//!
//! **What a record says.** Its severity (`WARN` or `ERROR`) and its callsite:
//! the event's target, the source file and line, and the NAMES of the fields
//! it was logged with (`message`, `error`, `pane_id`). Never the message text
//! and never a field's value, which can hold anything the daemon was handling:
//! a path, a pane title, a remote's answer. A source file outside the deck's
//! own tree is a path on the machine that built the binary, so only a relative
//! one is sent.
//!
//! **What it costs a callsite.** [`OtlpLogLayer::on_event`] runs inside the
//! `tracing` macro, wherever the daemon logs, some of it under locks the PTY
//! and doorbell paths rank. It does bounded, non-blocking work only: when the
//! export is live it copies the event's static metadata and the time into a
//! bounded channel with `try_send`, and when that channel is full it drops the
//! record and counts it. It takes no lock. Everything else — draining,
//! encoding, sending — happens on the export task, off the callsite.
//!
//! **When it is live.** The layer is installed with the file logger
//! ([`otlp_log_layer`], from the binary's `init_logging_from_env`) whenever
//! the process environment configures an OTLP logs destination, and does
//! nothing until the daemon starts its telemetry ([`start_for_daemon`]), so a
//! TUI or CLI process never exports. Its filter is its own ([`filter`]): WARN
//! and above, whatever `RUST_LOG` says, and nothing from the telemetry module
//! itself, so an export that fails cannot feed its own failure back in.
//!
//! **Best effort.** One attempt per batch, no retries: a batch the backend
//! does not take is lost, and so are records still in the channel when the
//! daemon stops. Records the channel dropped are reported in the next batch as
//! one record carrying `deck.telemetry.logs.dropped`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tracing::level_filters::LevelFilter;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::{Filtered, Targets};
use tracing_subscriber::layer::{Context, Layer};

use super::config::{
    self, DEFAULT_EXPORT_TIMEOUT, EnvMap, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_PROTOCOL,
    OTEL_EXPORTER_OTLP_TIMEOUT, OTEL_RESOURCE_ATTRIBUTES, OTEL_SERVICE_NAME, Protocol, non_empty,
};
use super::otlp::{SCOPE_NAME, json_kv, key_value, put_bytes, put_fixed64, put_varint_field};
use super::resource::{ResourceAttribute, deck_resource, host_name};
use super::span::AttrValue;
use super::{AttrKey, now_unix_nanos};

pub const OTEL_LOGS_EXPORTER: &str = "OTEL_LOGS_EXPORTER";
pub const OTEL_EXPORTER_OTLP_LOGS_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT";
pub const OTEL_EXPORTER_OTLP_LOGS_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL";
pub const OTEL_EXPORTER_OTLP_LOGS_HEADERS: &str = "OTEL_EXPORTER_OTLP_LOGS_HEADERS";
pub const OTEL_EXPORTER_OTLP_LOGS_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT";

/// Records waiting to be exported, at most. Past it a record is dropped and
/// counted.
pub const CHANNEL_CAPACITY: usize = 1024;

/// Records one export sends, at most.
pub const BATCH_MAX: usize = 512;

/// How often the export task sends what has accumulated.
pub const EXPORT_INTERVAL: Duration = Duration::from_secs(2);

/// The OTLP `SeverityNumber`s of the levels exported.
const SEVERITY_WARN: u64 = 13;
const SEVERITY_ERROR: u64 = 17;

/// Where the daemon's logs go: the OTLP logs variables, each falling back to
/// its generic `OTEL_EXPORTER_OTLP_*` form as the specification says.
#[derive(Clone, PartialEq, Eq)]
pub struct LogsConfig {
    pub endpoint: String,
    pub protocol: Protocol,
    /// Values are secrets.
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub service_name: String,
    pub user_resource_attributes: Vec<(String, String)>,
}

impl std::fmt::Debug for LogsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogsConfig")
            .field("endpoint", &config::redacted_endpoint(&self.endpoint))
            .field("protocol", &self.protocol)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl LogsConfig {
    /// The logs destination `env` describes, or `None`: the SDK is disabled,
    /// `OTEL_LOGS_EXPORTER` names no `otlp` exporter, no endpoint is set, or
    /// the protocol is one the deck cannot send (`grpc`).
    pub fn from_env(env: &EnvMap) -> Option<Self> {
        if config::sdk_disabled(env) {
            return None;
        }
        if let Some(exporters) = non_empty(env, OTEL_LOGS_EXPORTER)
            && !exporters
                .split(',')
                .any(|e| e.trim().eq_ignore_ascii_case("otlp"))
        {
            return None;
        }
        let endpoint = match non_empty(env, OTEL_EXPORTER_OTLP_LOGS_ENDPOINT) {
            Some(logs) => logs.to_string(),
            None => non_empty(env, OTEL_EXPORTER_OTLP_ENDPOINT)
                .map(|base| format!("{}/v1/logs", base.trim_end_matches('/')))?,
        };
        let protocol = non_empty(env, OTEL_EXPORTER_OTLP_LOGS_PROTOCOL)
            .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_PROTOCOL))
            .map(Protocol::parse)
            .unwrap_or(Protocol::HttpProtobuf);
        if !protocol.deck_can_export() {
            return None;
        }
        let timeout = non_empty(env, OTEL_EXPORTER_OTLP_LOGS_TIMEOUT)
            .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_TIMEOUT))
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_EXPORT_TIMEOUT);
        Some(LogsConfig {
            endpoint,
            protocol,
            headers: config::resolve_headers_for(env, OTEL_EXPORTER_OTLP_LOGS_HEADERS),
            timeout,
            service_name: non_empty(env, OTEL_SERVICE_NAME)
                .unwrap_or(super::DEFAULT_SERVICE_NAME)
                .to_string(),
            user_resource_attributes: non_empty(env, OTEL_RESOURCE_ATTRIBUTES)
                .map(super::resource::parse_resource_attributes)
                .unwrap_or_default(),
        })
    }
}

/// One logged event: its callsite, which is static, and when it happened.
#[derive(Debug, Clone, Copy)]
pub struct Record {
    pub metadata: &'static Metadata<'static>,
    pub time_unix_nanos: u64,
}

/// The channel between the layer and the export task.
#[derive(Debug)]
pub struct LogChannel {
    tx: SyncSender<Record>,
    rx: Mutex<Option<Receiver<Record>>>,
    live: AtomicBool,
    dropped: AtomicU64,
}

impl LogChannel {
    pub fn new(capacity: usize) -> Arc<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
        Arc::new(LogChannel {
            tx,
            rx: Mutex::new(Some(rx)),
            live: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        })
    }

    /// Start accepting records, and hand over the receiving end — once.
    fn go_live(&self) -> Option<Receiver<Record>> {
        let rx = self.rx.lock().unwrap_or_else(|p| p.into_inner()).take()?;
        self.live.store(true, Ordering::Release);
        Some(rx)
    }

    /// Records dropped since the last call.
    fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

/// The process's channel, made by the first [`otlp_log_layer`].
static CHANNEL: OnceLock<Arc<LogChannel>> = OnceLock::new();

/// The `tracing` layer that hands the daemon's warnings and errors to the
/// export task. See the module docs for what it may do.
#[derive(Debug, Clone)]
pub struct OtlpLogLayer {
    channel: Arc<LogChannel>,
}

impl OtlpLogLayer {
    pub fn new(channel: Arc<LogChannel>) -> Self {
        OtlpLogLayer { channel }
    }
}

impl<S: Subscriber> Layer<S> for OtlpLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !self.channel.live.load(Ordering::Acquire) {
            return;
        }
        let record = Record {
            metadata: event.metadata(),
            time_unix_nanos: now_unix_nanos(),
        };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.channel.tx.try_send(record)
        {
            self.channel.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What the layer sees: WARN and above from every target but the telemetry
/// module's own, whatever `RUST_LOG` says. Separate from the file logger's
/// filter, which this does not change.
pub fn filter() -> Targets {
    Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("dot_agent_deck::telemetry", LevelFilter::OFF)
}

/// The layer to install beside the file logger, filtered by [`filter`], when
/// the process environment configures an OTLP logs destination. It stays idle
/// until [`start_for_daemon`].
pub fn otlp_log_layer<S>() -> Option<Filtered<OtlpLogLayer, Targets, S>>
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    LogsConfig::from_env(&config::process_env())?;
    let channel = Arc::clone(CHANNEL.get_or_init(|| LogChannel::new(CHANNEL_CAPACITY)));
    Some(OtlpLogLayer::new(channel).with_filter(filter()))
}

/// The daemon started its telemetry: start exporting its warnings and errors
/// to the destination `env` configures, when the layer was installed. Nothing
/// happens otherwise, or on a second call.
pub fn start_for_daemon(env: &EnvMap) {
    let (Some(channel), Some(config)) = (CHANNEL.get(), LogsConfig::from_env(env)) else {
        return;
    };
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let Some(rx) = channel.go_live() else {
        return;
    };
    let channel = Arc::clone(channel);
    let host = host_name();
    tokio::spawn(export(channel, rx, config, host));
}

/// The export task: every [`EXPORT_INTERVAL`], send what accumulated.
async fn export(
    channel: Arc<LogChannel>,
    rx: Receiver<Record>,
    config: LogsConfig,
    host: Option<String>,
) {
    let resource = deck_resource(
        &config.service_name,
        host.as_deref(),
        &config.user_resource_attributes,
    );
    let Ok(client) = reqwest::Client::builder()
        .timeout(config.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(super::exporter::build_headers(&config.headers))
        .build()
    else {
        tracing::warn!("telemetry: the OTLP logs client could not be built; no logs are sent");
        return;
    };
    tracing::info!(
        endpoint = %config::redacted_endpoint(&config.endpoint),
        "telemetry: exporting the daemon's warnings and errors as OTLP logs"
    );
    let mut tick = tokio::time::interval(EXPORT_INTERVAL);
    loop {
        tick.tick().await;
        loop {
            let records: Vec<Record> = rx.try_iter().take(BATCH_MAX).collect();
            let dropped = channel.take_dropped();
            if records.is_empty() && dropped == 0 {
                break;
            }
            let full = records.len() == BATCH_MAX;
            let (body, content_type) = match config.protocol {
                Protocol::HttpJson => (
                    encode_json(&resource, &records, dropped, now_unix_nanos()),
                    "application/json",
                ),
                _ => (
                    encode_protobuf(&resource, &records, dropped, now_unix_nanos()),
                    "application/x-protobuf",
                ),
            };
            let sent = client
                .post(&config.endpoint)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body)
                .send()
                .await;
            match sent {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => tracing::debug!(
                    status = response.status().as_u16(),
                    "telemetry: the OTLP backend did not take a batch of logs"
                ),
                Err(_) => tracing::debug!("telemetry: a batch of OTLP logs could not be sent"),
            }
            if !full {
                break;
            }
        }
    }
}

fn severity(level: &Level) -> (u64, &'static str) {
    if *level == Level::ERROR {
        (SEVERITY_ERROR, "ERROR")
    } else {
        (SEVERITY_WARN, "WARN")
    }
}

/// A record's attributes: its callsite and its fields' names.
fn attributes(metadata: &Metadata<'static>) -> Vec<(AttrKey, AttrValue)> {
    let mut attributes = vec![(
        AttrKey::LogTarget,
        AttrValue::Str(metadata.target().to_string()),
    )];
    if let Some(file) = relative_source(metadata.file()) {
        attributes.push((AttrKey::CodeFilePath, AttrValue::Str(file.to_string())));
    }
    if let Some(line) = metadata.line() {
        attributes.push((AttrKey::CodeLineNumber, AttrValue::Int(i64::from(line))));
    }
    let names: Vec<&str> = metadata.fields().iter().map(|f| f.name()).collect();
    if !names.is_empty() {
        attributes.push((AttrKey::LogFields, AttrValue::Str(names.join(","))));
    }
    attributes
}

/// `file` when it is relative to its crate. One outside the deck's tree is
/// absolute, a path on the machine that built the binary, and is left out.
fn relative_source(file: Option<&str>) -> Option<&str> {
    file.filter(|f| !f.starts_with('/') && !f.contains(':'))
}

/// The record reporting `dropped` records the channel had no room for.
fn dropped_attributes(dropped: u64) -> Vec<(AttrKey, AttrValue)> {
    vec![(
        AttrKey::LogsDropped,
        AttrValue::Int(i64::try_from(dropped).unwrap_or(i64::MAX)),
    )]
}

/// One record as it is encoded: its time, its severity's number and text, and
/// its attributes.
type Row = (u64, (u64, &'static str), Vec<(AttrKey, AttrValue)>);

/// Every record's [`Row`], the dropped count's last.
fn rows(records: &[Record], dropped: u64, now: u64) -> Vec<Row> {
    let mut rows: Vec<_> = records
        .iter()
        .map(|r| {
            (
                r.time_unix_nanos,
                severity(r.metadata.level()),
                attributes(r.metadata),
            )
        })
        .collect();
    if dropped > 0 {
        rows.push((now, (SEVERITY_WARN, "WARN"), dropped_attributes(dropped)));
    }
    rows
}

/// An `ExportLogsServiceRequest` for `http/protobuf`. Field numbers are those
/// of `opentelemetry/proto/logs/v1/logs.proto`.
pub fn encode_protobuf(
    resource: &[ResourceAttribute],
    records: &[Record],
    dropped: u64,
    now: u64,
) -> Vec<u8> {
    let mut resource_msg = Vec::new();
    for attr in resource {
        put_bytes(&mut resource_msg, 1, &key_value(attr.key(), &attr.value()));
    }
    let mut scope = Vec::new();
    put_bytes(&mut scope, 1, SCOPE_NAME.as_bytes());
    put_bytes(&mut scope, 2, env!("DAD_VERSION").as_bytes());
    let mut scope_logs = Vec::new();
    put_bytes(&mut scope_logs, 1, &scope);
    for (time, (number, text), attributes) in rows(records, dropped, now) {
        let mut record = Vec::new();
        put_fixed64(&mut record, 1, time);
        put_varint_field(&mut record, 2, number);
        put_bytes(&mut record, 3, text.as_bytes());
        for (key, value) in &attributes {
            put_bytes(&mut record, 6, &key_value(key.as_str(), value));
        }
        put_fixed64(&mut record, 11, now);
        put_bytes(&mut scope_logs, 2, &record);
    }
    let mut resource_logs = Vec::new();
    put_bytes(&mut resource_logs, 1, &resource_msg);
    put_bytes(&mut resource_logs, 2, &scope_logs);
    let mut request = Vec::new();
    put_bytes(&mut request, 1, &resource_logs);
    request
}

/// The OTLP/JSON body for `http/json`.
pub fn encode_json(
    resource: &[ResourceAttribute],
    records: &[Record],
    dropped: u64,
    now: u64,
) -> Vec<u8> {
    let records: Vec<serde_json::Value> = rows(records, dropped, now)
        .into_iter()
        .map(|(time, (number, text), attributes)| {
            serde_json::json!({
                "timeUnixNano": time.to_string(),
                "observedTimeUnixNano": now.to_string(),
                "severityNumber": number,
                "severityText": text,
                "attributes": attributes.iter()
                    .map(|(key, value)| json_kv(key.as_str(), value))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let body = serde_json::json!({
        "resourceLogs": [{
            "resource": {
                "attributes": resource.iter()
                    .map(|a| json_kv(a.key(), &a.value()))
                    .collect::<Vec<_>>(),
            },
            "scopeLogs": [{
                "scope": { "name": SCOPE_NAME, "version": env!("DAD_VERSION") },
                "logRecords": records,
            }],
        }],
    });
    serde_json::to_vec(&body).expect("a JSON value always serializes")
}

/// A subscriber with only `layer`, for tests that log through it.
#[cfg(test)]
fn subscriber_with(layer: OtlpLogLayer) -> impl Subscriber + Send + Sync {
    use tracing_subscriber::layer::SubscriberExt as _;
    tracing_subscriber::registry().with(layer.with_filter(filter()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "PRIVATE_LOG_VALUE_1981_c0ffee";
    /// A target outside the telemetry module, which the filter leaves out.
    const DAEMON: &str = "dot_agent_deck::daemon";

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Run `f` with only `subscriber` installed, through the crate's seam
    /// for unit-test captures.
    fn log_through(subscriber: impl Subscriber + Send + Sync + 'static, f: impl FnOnce()) {
        let _guard = crate::test_isolation::capture_tracing_on_this_thread(subscriber);
        f();
    }

    fn live(capacity: usize) -> (Arc<LogChannel>, Receiver<Record>) {
        let channel = LogChannel::new(capacity);
        let rx = channel.go_live().unwrap();
        (channel, rx)
    }

    #[test]
    fn the_destination_follows_the_logs_variables_then_the_generic_ones() {
        let base = [("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318/")];
        let config = LogsConfig::from_env(&env(&base)).unwrap();
        assert_eq!(config.endpoint, "http://c:4318/v1/logs");
        assert_eq!(config.protocol, Protocol::HttpProtobuf);
        let config = LogsConfig::from_env(&env(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            (OTEL_EXPORTER_OTLP_LOGS_ENDPOINT, "http://l/logs"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "a=1"),
            (OTEL_EXPORTER_OTLP_LOGS_HEADERS, "b=2"),
        ]))
        .unwrap();
        assert_eq!(config.endpoint, "http://l/logs");
        assert_eq!(config.protocol, Protocol::HttpJson);
        assert_eq!(config.headers, vec![("b".to_string(), "2".to_string())]);
        // A traces-only endpoint is not a logs destination.
        assert!(
            LogsConfig::from_env(&env(&[(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://t/v1/traces"
            )]))
            .is_none()
        );
        for off in [
            (OTEL_LOGS_EXPORTER, "none"),
            ("OTEL_SDK_DISABLED", "true"),
            (OTEL_EXPORTER_OTLP_LOGS_PROTOCOL, "grpc"),
        ] {
            let mut e = env(&base);
            e.insert(off.0.into(), off.1.into());
            assert!(LogsConfig::from_env(&e).is_none(), "{off:?}");
        }
        let debug = format!(
            "{:?}",
            LogsConfig::from_env(&env(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c"),
                ("OTEL_EXPORTER_OTLP_HEADERS", &format!("k={SECRET}")),
            ]))
            .unwrap()
        );
        assert!(!debug.contains(SECRET), "{debug}");
    }

    /// Only WARN and ERROR reach the channel, nothing from the telemetry
    /// module, and nothing before the export is live.
    #[test]
    fn the_layer_takes_warnings_and_errors_only_once_live() {
        let channel = LogChannel::new(16);
        log_through(
            subscriber_with(OtlpLogLayer::new(Arc::clone(&channel))),
            || {
                tracing::warn!(target: DAEMON, "before the export is live");
            },
        );
        let rx = channel.go_live().unwrap();
        log_through(
            subscriber_with(OtlpLogLayer::new(Arc::clone(&channel))),
            || {
                tracing::info!(target: DAEMON, "not exported");
                tracing::warn!(target: DAEMON, pane_id = SECRET, "a warning about {SECRET}");
                tracing::error!(target: DAEMON, error = SECRET, "an error");
                tracing::warn!(target: "dot_agent_deck::telemetry::exporter", "its own failure");
            },
        );
        let records: Vec<Record> = rx.try_iter().collect();
        assert_eq!(records.len(), 2, "{records:?}");
        assert_eq!(*records[0].metadata.level(), Level::WARN);
        assert_eq!(*records[1].metadata.level(), Level::ERROR);
        assert_eq!(channel.take_dropped(), 0);
    }

    /// The callsite and the fields' names are sent; the message text and the
    /// fields' values never are.
    #[test]
    fn a_record_carries_its_callsite_and_field_names_not_its_values() {
        let (channel, rx) = live(16);
        log_through(subscriber_with(OtlpLogLayer::new(channel)), || {
            tracing::warn!(target: DAEMON, pane_id = SECRET, reason = %SECRET, "could not follow {SECRET}");
        });
        let records: Vec<Record> = rx.try_iter().collect();
        let resource = deck_resource("dot-agent-deck", Some("h"), &[]);
        let json = encode_json(&resource, &records, 0, 5);
        let protobuf = encode_protobuf(&resource, &records, 0, 5);
        for body in [&json, &protobuf] {
            assert!(
                !body.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
                "a value or the message reached the body"
            );
        }
        let body: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let record = &body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["severityNumber"], SEVERITY_WARN);
        assert_eq!(record["severityText"], "WARN");
        let attr = |key: &str| {
            record["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == key)
                .map(|a| a["value"].clone())
        };
        assert_eq!(
            attr("deck.log.fields").unwrap()["stringValue"],
            "message,pane_id,reason"
        );
        assert_eq!(attr("deck.log.target").unwrap()["stringValue"], DAEMON);
        assert_eq!(
            attr("code.file.path").unwrap()["stringValue"],
            "src/telemetry/logs.rs"
        );
        assert!(attr("code.line.number").is_some());
        assert!(record.get("body").is_none());
    }

    /// Qodo/dispatcher condition: a full channel never makes a callsite wait.
    /// Logging into one that is full and never drained returns at once, and
    /// every record that did not fit is counted.
    #[test]
    fn a_full_channel_drops_and_counts_without_blocking() {
        let (channel, rx) = live(4);
        let started = std::time::Instant::now();
        log_through(
            subscriber_with(OtlpLogLayer::new(Arc::clone(&channel))),
            || {
                for i in 0..10_000 {
                    tracing::warn!(target: DAEMON, i, "flood");
                }
            },
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "logging waited on the channel: {:?}",
            started.elapsed()
        );
        assert_eq!(channel.take_dropped(), 10_000 - 4);
        assert_eq!(rx.try_iter().count(), 4);
        // The count is reported once, as its own record.
        let resource = deck_resource("dot-agent-deck", None, &[]);
        let body: serde_json::Value =
            serde_json::from_slice(&encode_json(&resource, &[], 9996, 7)).unwrap();
        let records = body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
            .as_array()
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0]["attributes"][0],
            serde_json::json!({"key": "deck.telemetry.logs.dropped", "value": {"intValue": "9996"}})
        );
    }

    /// The receiving end is handed out once, so a second start does nothing.
    #[test]
    fn the_export_goes_live_once() {
        let channel = LogChannel::new(1);
        assert!(channel.go_live().is_some());
        assert!(channel.go_live().is_none());
    }

    #[test]
    fn an_absolute_source_path_is_left_out() {
        assert_eq!(
            relative_source(Some("src/daemon.rs")),
            Some("src/daemon.rs")
        );
        assert_eq!(
            relative_source(Some("/home/someone/.cargo/registry/src/hyper/lib.rs")),
            None
        );
        assert_eq!(relative_source(Some("C:\\cargo\\hyper\\lib.rs")), None);
        assert_eq!(relative_source(None), None);
    }
}
