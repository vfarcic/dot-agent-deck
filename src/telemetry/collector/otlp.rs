//! The OTLP `ExportMetricsServiceRequest` the collector sends for one sample,
//! and its OTLP/JSON and protobuf encodings.
//!
//! Each run, the unattributed bucket and each measured worktree is its own
//! resource, so `deck.run.id` sits on the resource exactly as it does in the
//! agents' own telemetry, and a backend joins the two on it.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{Value, json};

use super::aggregate::{Figures, RunKey, Sample};
use super::config::{Settings, Temporality};
use super::{SCHEMA_VERSION, SCOPE_NAME, keys, metrics};
use crate::telemetry::otlp::{put_bytes, put_fixed64, put_varint_field};

/// A worktree as the collector names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRef {
    pub id: String,
    /// Its canonical path, only when the user opted into sending paths.
    pub path: Option<String>,
}

/// One completed walk of a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeUsage {
    pub worktree: WorktreeRef,
    pub bytes: u64,
    /// When the walk completed (Unix nanoseconds).
    pub measured_unix_nanos: u64,
    /// `false` when the walk stopped at a limit, so `bytes` is a lower
    /// bound; sent as `deck.worktree.disk.complete` = `"false"`.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub resources: Vec<ResourceMetrics>,
}

impl Request {
    /// How many data points the request carries.
    pub fn data_points(&self) -> usize {
        self.resources
            .iter()
            .flat_map(|r| &r.metrics)
            .map(|m| match &m.data {
                MetricData::Sum { points, .. } | MetricData::Gauge { points } => points.len(),
            })
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceMetrics {
    pub attributes: Vec<(String, String)>,
    pub metrics: Vec<Metric>,
    /// The last report of a run that ended, which [`PendingTerminals`] keeps
    /// until a request carrying it is delivered.
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub name: &'static str,
    pub unit: &'static str,
    pub description: &'static str,
    pub data: MetricData,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetricData {
    Sum {
        temporality: Temporality,
        monotonic: bool,
        points: Vec<NumberPoint>,
    },
    Gauge {
        points: Vec<NumberPoint>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NumberPoint {
    pub start_unix_nanos: Option<u64>,
    pub time_unix_nanos: u64,
    pub value: NumberValue,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumberValue {
    Double(f64),
    Int(i64),
}

fn kv(key: &str, value: &str) -> (String, String) {
    (key.to_string(), value.to_string())
}

fn int(value: u64) -> NumberValue {
    NumberValue::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

/// What every resource carries: the service, its version, the host, the
/// schema version, then the collector's own filtered pairs.
pub fn base_resource(settings: &Settings, host_name: Option<&str>) -> Vec<(String, String)> {
    let mut out = vec![
        kv("service.name", &settings.service_name),
        kv("service.version", env!("DAD_VERSION")),
    ];
    if let Some(host) = host_name.filter(|h| !h.is_empty()) {
        out.push(kv("host.name", host));
    }
    out.push(kv(keys::SCHEMA_VERSION, SCHEMA_VERSION));
    out.extend(settings.resource_pairs.iter().cloned());
    out
}

fn worktree_attributes(worktree: &WorktreeRef) -> Vec<(String, String)> {
    let mut out = vec![kv(keys::WORKTREE_ID, &worktree.id)];
    if let Some(path) = &worktree.path {
        out.push(kv(keys::WORKTREE_PATH, path));
    }
    out
}

/// The CPU sum, memory gauge and process count of a run or of the
/// unattributed bucket.
fn usage_metrics(figures: &Figures, time: u64, temporality: Temporality) -> Vec<Metric> {
    let (start, cpu) = match temporality {
        Temporality::Cumulative => (figures.start_unix_nanos, figures.cpu_seconds),
        Temporality::Delta => (
            figures
                .previous_unix_nanos
                .unwrap_or(figures.start_unix_nanos),
            figures.cpu_seconds_delta,
        ),
    };
    let pss = [
        ("anon", figures.pss.anon),
        ("file", figures.pss.file),
        ("shmem", figures.pss.shmem),
    ];
    vec![
        Metric {
            name: metrics::CPU_TIME,
            unit: "s",
            description: "CPU time used by the run's processes",
            data: MetricData::Sum {
                temporality,
                monotonic: true,
                points: vec![NumberPoint {
                    start_unix_nanos: Some(start),
                    time_unix_nanos: time,
                    value: NumberValue::Double(cpu),
                    attributes: Vec::new(),
                }],
            },
        },
        Metric {
            name: metrics::MEMORY_PSS,
            unit: "By",
            description: "Proportional set size of the run's processes",
            data: MetricData::Gauge {
                points: pss
                    .iter()
                    .map(|(kind, bytes)| NumberPoint {
                        start_unix_nanos: None,
                        time_unix_nanos: time,
                        value: int(*bytes),
                        attributes: vec![kv(keys::MEMORY_KIND, kind)],
                    })
                    .collect(),
            },
        },
        Metric {
            name: metrics::PROCESSES,
            unit: "{process}",
            description: "Live processes attributed to the run",
            data: MetricData::Gauge {
                points: vec![NumberPoint {
                    start_unix_nanos: None,
                    time_unix_nanos: time,
                    value: int(figures.processes),
                    attributes: Vec::new(),
                }],
            },
        },
    ]
}

/// The request for one sample: a resource per run (with its worktree from
/// `run_worktrees` when it has one), the unattributed bucket, then a resource
/// per worktree in `disks`.
pub fn build_request(
    sample: &Sample,
    run_worktrees: &HashMap<RunKey, WorktreeRef>,
    disks: &[WorktreeUsage],
    settings: &Settings,
    host_name: Option<&str>,
) -> Request {
    let base = base_resource(settings, host_name);
    let mut resources = Vec::with_capacity(sample.runs.len() + 1 + disks.len());
    for run in &sample.runs {
        let mut attributes = base.clone();
        attributes.push(kv(keys::ATTRIBUTION, "run"));
        attributes.push(kv(keys::RUN_ID, &run.identity.run_id));
        attributes.extend(run.identity.attributes.iter().cloned());
        if settings.label_uids {
            attributes.push(kv(keys::RUN_UID, &run.uid.to_string()));
        }
        if let Some(worktree) = run_worktrees.get(&run.key()) {
            attributes.extend(worktree_attributes(worktree));
        }
        resources.push(ResourceMetrics {
            attributes,
            metrics: usage_metrics(&run.figures, sample.time_unix_nanos, settings.temporality),
            terminal: run.ended,
        });
    }
    let mut attributes = base.clone();
    attributes.push(kv(keys::ATTRIBUTION, "unattributed"));
    resources.push(ResourceMetrics {
        attributes,
        metrics: usage_metrics(
            &sample.unattributed,
            sample.time_unix_nanos,
            settings.temporality,
        ),
        terminal: false,
    });
    for disk in disks {
        let mut attributes = base.clone();
        attributes.extend(worktree_attributes(&disk.worktree));
        resources.push(ResourceMetrics {
            attributes,
            metrics: vec![Metric {
                name: metrics::DISK_USAGE,
                unit: "By",
                description: "Disk space allocated under the worktree",
                data: MetricData::Gauge {
                    points: vec![NumberPoint {
                        start_unix_nanos: None,
                        time_unix_nanos: disk.measured_unix_nanos,
                        value: int(disk.bytes),
                        attributes: if disk.complete {
                            Vec::new()
                        } else {
                            vec![kv(keys::DISK_COMPLETE, "false")]
                        },
                    }],
                },
            }],
            terminal: false,
        });
    }
    Request { resources }
}

/// Each run's worktree, decided at the first sample that sees the run: the
/// worktree of its oldest live process that has one, or none when none of
/// them is in a worktree then. It does not change after that — not when that
/// process exits, another changes directory, or a run that had none enters a
/// worktree — and it is kept through the run's last report, so every point
/// of a run, its terminal ones included, carries the same resource
/// attributes.
#[derive(Debug, Default)]
pub struct RunWorktrees {
    map: HashMap<RunKey, WorktreeRef>,
    /// Every run whose worktree is decided, those with none included.
    decided: HashSet<RunKey>,
}

impl RunWorktrees {
    /// Decide the worktree of each run in `sample` seen for the first time:
    /// the first one `worktree_of` names for its members, oldest first, or
    /// none.
    pub fn update(
        &mut self,
        sample: &Sample,
        mut worktree_of: impl FnMut(u32) -> Option<WorktreeRef>,
    ) {
        for run in &sample.runs {
            let key = run.key();
            if self.decided.contains(&key) {
                continue;
            }
            if let Some(worktree) = run.members.iter().find_map(|pid| worktree_of(*pid)) {
                self.map.insert(key.clone(), worktree);
            }
            self.decided.insert(key);
        }
    }

    pub fn get(&self) -> &HashMap<RunKey, WorktreeRef> {
        &self.map
    }

    /// Whether the worktree of the run `key` is decided, as having none
    /// included: a run that is not is decided at the next [`Self::update`].
    pub fn is_decided(&self, key: &RunKey) -> bool {
        self.decided.contains(key)
    }

    /// Forget the runs that ended in `sample`, once its request is built.
    pub fn forget_ended(&mut self, sample: &Sample) {
        for run in sample.runs.iter().filter(|r| r.ended) {
            self.map.remove(&run.key());
            self.decided.remove(&run.key());
        }
    }
}

/// Terminal resources held for a later request, at most. Past it the oldest
/// is dropped and counted.
pub const MAX_PENDING_TERMINALS: usize = 1024;

/// The last reports of ended runs whose request was not delivered: each is
/// added to the next request until one is, so a run's final figures reach
/// the backend when a later send succeeds. A live run needs no such care:
/// its next report carries its running total.
#[derive(Debug, Default)]
pub struct PendingTerminals {
    /// Oldest first.
    held: VecDeque<ResourceMetrics>,
    /// Where [`Self::attach`] put the held resources in the request it was
    /// last given: everything before it is the request's own.
    attached_at: Option<usize>,
    /// Terminal resources dropped because more than
    /// [`MAX_PENDING_TERMINALS`] were held.
    pub dropped: u64,
}

impl PendingTerminals {
    /// Add the held terminal resources to the end of `request`.
    pub fn attach(&mut self, request: &mut Request) {
        self.attached_at = Some(request.resources.len());
        request.resources.extend(self.held.iter().cloned());
    }

    /// `request`, with what [`Self::attach`] added, was delivered.
    pub fn delivered(&mut self) {
        self.held.clear();
        self.attached_at = None;
    }

    /// `request` was not delivered: hold its own terminal resources after
    /// those already held (which [`Self::attach`] added to it, or did not),
    /// and drop the oldest past [`MAX_PENDING_TERMINALS`]; returns how many
    /// were dropped.
    pub fn failed(&mut self, request: &Request) -> u64 {
        let own = self
            .attached_at
            .take()
            .unwrap_or(request.resources.len())
            .min(request.resources.len());
        self.held.extend(
            request.resources[..own]
                .iter()
                .filter(|r| r.terminal)
                .cloned(),
        );
        let excess = self.held.len().saturating_sub(MAX_PENDING_TERMINALS);
        self.held.drain(..excess);
        self.dropped += excess as u64;
        excess as u64
    }

    /// Hold `request`'s terminal resources for a later request without
    /// sending it: the collector's sample in progress at shutdown, whose
    /// request the final one replaces. Returns how many were dropped past
    /// the bound, as [`Self::failed`] does.
    pub fn hold(&mut self, request: &Request) -> u64 {
        self.attached_at = None;
        self.failed(request)
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

fn json_attributes(attributes: &[(String, String)]) -> Value {
    Value::Array(
        attributes
            .iter()
            .map(|(k, v)| json!({ "key": k, "value": { "stringValue": v } }))
            .collect(),
    )
}

fn json_point(point: &NumberPoint) -> Value {
    let mut value = json!({
        "timeUnixNano": point.time_unix_nanos.to_string(),
        "attributes": json_attributes(&point.attributes),
    });
    if let Some(start) = point.start_unix_nanos {
        value["startTimeUnixNano"] = Value::String(start.to_string());
    }
    match point.value {
        NumberValue::Double(d) => value["asDouble"] = json!(d),
        // OTLP/JSON writes 64-bit integers as decimal strings.
        NumberValue::Int(i) => value["asInt"] = Value::String(i.to_string()),
    }
    value
}

fn json_metric(metric: &Metric) -> Value {
    let mut value = json!({
        "name": metric.name,
        "unit": metric.unit,
        "description": metric.description,
    });
    match &metric.data {
        MetricData::Sum {
            temporality,
            monotonic,
            points,
        } => {
            value["sum"] = json!({
                "aggregationTemporality": temporality.otlp_value(),
                "isMonotonic": monotonic,
                "dataPoints": points.iter().map(json_point).collect::<Vec<_>>(),
            });
        }
        MetricData::Gauge { points } => {
            value["gauge"] = json!({
                "dataPoints": points.iter().map(json_point).collect::<Vec<_>>(),
            });
        }
    }
    value
}

/// The OTLP/JSON body for `http/json`, and the line `--stdout` prints.
pub fn encode_json(request: &Request) -> Vec<u8> {
    let resources: Vec<Value> = request
        .resources
        .iter()
        .map(|r| {
            json!({
                "resource": { "attributes": json_attributes(&r.attributes) },
                "scopeMetrics": [{
                    "scope": { "name": SCOPE_NAME, "version": env!("DAD_VERSION") },
                    "metrics": r.metrics.iter().map(json_metric).collect::<Vec<_>>(),
                }],
            })
        })
        .collect();
    serde_json::to_vec(&json!({ "resourceMetrics": resources }))
        .expect("a JSON value always serializes")
}

// Field numbers are those of `opentelemetry/proto/metrics/v1/metrics.proto`,
// `common/v1/common.proto`, `resource/v1/resource.proto` and
// `collector/metrics/v1/metrics_service.proto`.

fn proto_key_value(key: &str, value: &str) -> Vec<u8> {
    let mut any = Vec::new();
    put_bytes(&mut any, 1, value.as_bytes());
    let mut buf = Vec::new();
    put_bytes(&mut buf, 1, key.as_bytes());
    put_bytes(&mut buf, 2, &any);
    buf
}

fn proto_point(point: &NumberPoint) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(start) = point.start_unix_nanos {
        put_fixed64(&mut buf, 2, start);
    }
    put_fixed64(&mut buf, 3, point.time_unix_nanos);
    match point.value {
        NumberValue::Double(d) => put_fixed64(&mut buf, 4, d.to_bits()),
        // `as_int` is an `sfixed64`: the same eight little-endian bytes.
        NumberValue::Int(i) => put_fixed64(&mut buf, 6, i as u64),
    }
    for (key, value) in &point.attributes {
        put_bytes(&mut buf, 7, &proto_key_value(key, value));
    }
    buf
}

fn proto_metric(metric: &Metric) -> Vec<u8> {
    let mut buf = Vec::new();
    put_bytes(&mut buf, 1, metric.name.as_bytes());
    put_bytes(&mut buf, 2, metric.description.as_bytes());
    put_bytes(&mut buf, 3, metric.unit.as_bytes());
    match &metric.data {
        MetricData::Gauge { points } => {
            let mut gauge = Vec::new();
            for point in points {
                put_bytes(&mut gauge, 1, &proto_point(point));
            }
            put_bytes(&mut buf, 5, &gauge);
        }
        MetricData::Sum {
            temporality,
            monotonic,
            points,
        } => {
            let mut sum = Vec::new();
            for point in points {
                put_bytes(&mut sum, 1, &proto_point(point));
            }
            put_varint_field(&mut sum, 2, u64::from(temporality.otlp_value()));
            if *monotonic {
                put_varint_field(&mut sum, 3, 1);
            }
            put_bytes(&mut buf, 7, &sum);
        }
    }
    buf
}

/// The protobuf body for `http/protobuf`: the same request [`encode_json`]
/// describes.
pub fn encode_protobuf(request: &Request) -> Vec<u8> {
    let mut scope = Vec::new();
    put_bytes(&mut scope, 1, SCOPE_NAME.as_bytes());
    put_bytes(&mut scope, 2, env!("DAD_VERSION").as_bytes());
    let mut body = Vec::new();
    for r in &request.resources {
        let mut resource = Vec::new();
        for (key, value) in &r.attributes {
            put_bytes(&mut resource, 1, &proto_key_value(key, value));
        }
        let mut scope_metrics = Vec::new();
        put_bytes(&mut scope_metrics, 1, &scope);
        for metric in &r.metrics {
            put_bytes(&mut scope_metrics, 2, &proto_metric(metric));
        }
        let mut resource_metrics = Vec::new();
        put_bytes(&mut resource_metrics, 1, &resource);
        put_bytes(&mut resource_metrics, 2, &scope_metrics);
        put_bytes(&mut body, 1, &resource_metrics);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::collector::aggregate::RunFigures;
    use crate::telemetry::collector::identity::RunIdentity;
    use crate::telemetry::collector::procfs::Pss;

    const T: u64 = 1_792_890_015_000_000_000;

    fn settings(temporality: Temporality) -> Settings {
        Settings {
            service_name: "dot-agent-deck-collector".into(),
            resource_pairs: vec![("team".into(), "x".into())],
            temporality,
            unknown_temporality: None,
            label_uids: false,
        }
    }

    fn figures(cpu: f64, delta: f64, processes: u64) -> Figures {
        Figures {
            cpu_seconds: cpu,
            cpu_seconds_delta: delta,
            start_unix_nanos: 1_792_890_000_000_000_000,
            previous_unix_nanos: Some(T - 15_000_000_000),
            pss: Pss {
                anon: 502_407_168,
                file: 226_168_832,
                shmem: 0,
            },
            processes,
        }
    }

    fn sample() -> Sample {
        Sample {
            time_unix_nanos: T,
            runs: vec![RunFigures {
                identity: RunIdentity {
                    run_id: "run-1".into(),
                    attributes: vec![("deck.pane.id".into(), "3".into())],
                },
                uid: 1000,
                figures: figures(4.02, 0.5, 6),
                ended: false,
                members: vec![10, 11],
            }],
            unattributed: figures(1.0, 0.25, 2),
        }
    }

    fn run_1() -> RunKey {
        RunKey {
            run_id: "run-1".into(),
            uid: 1000,
        }
    }

    fn worktree(path: Option<&str>) -> WorktreeRef {
        WorktreeRef {
            id: "5d41402abc4b2a76b9719d911017c592".into(),
            path: path.map(str::to_string),
        }
    }

    fn encoded(temporality: Temporality, path: Option<&str>) -> Value {
        let worktrees = HashMap::from([(run_1(), worktree(path))]);
        let disks = [WorktreeUsage {
            worktree: worktree(path),
            bytes: 170_864_640,
            measured_unix_nanos: T - 1,
            complete: true,
        }];
        let request = build_request(
            &sample(),
            &worktrees,
            &disks,
            &settings(temporality),
            Some("studio"),
        );
        serde_json::from_slice(&encode_json(&request)).unwrap()
    }

    fn attrs(resource: &Value) -> Vec<(String, String)> {
        resource["resource"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["key"].as_str().unwrap().to_string(),
                    a["value"]["stringValue"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn metric<'a>(resource: &'a Value, name: &str) -> &'a Value {
        resource["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resources_are_runs_then_unattributed_then_worktrees() {
        let body = encoded(Temporality::Cumulative, None);
        let resources = body["resourceMetrics"].as_array().unwrap();
        assert_eq!(resources.len(), 3);
        let base = pairs(&[
            ("service.name", "dot-agent-deck-collector"),
            ("service.version", env!("DAD_VERSION")),
            ("host.name", "studio"),
            ("deck.collector.schema_version", "1"),
            ("team", "x"),
        ]);
        let mut run = base.clone();
        run.extend(pairs(&[
            ("deck.attribution", "run"),
            ("deck.run.id", "run-1"),
            ("deck.pane.id", "3"),
            ("deck.worktree.id", "5d41402abc4b2a76b9719d911017c592"),
        ]));
        assert_eq!(attrs(&resources[0]), run);
        let mut unattributed = base.clone();
        unattributed.push(("deck.attribution".into(), "unattributed".into()));
        assert_eq!(attrs(&resources[1]), unattributed);
        let mut wt = base.clone();
        wt.push((
            "deck.worktree.id".into(),
            "5d41402abc4b2a76b9719d911017c592".into(),
        ));
        assert_eq!(attrs(&resources[2]), wt);
        for r in resources {
            assert_eq!(
                r["scopeMetrics"][0]["scope"]["name"],
                "dot-agent-deck-collector"
            );
            assert_eq!(
                r["scopeMetrics"][0]["scope"]["version"],
                env!("DAD_VERSION")
            );
        }
        let names: Vec<_> = resources[0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "deck.run.cpu.time",
                "deck.run.memory.pss",
                "deck.run.processes"
            ]
        );
        assert_eq!(
            metric(&resources[2], "deck.worktree.disk.usage")["gauge"]["dataPoints"][0],
            json!({"timeUnixNano": (T - 1).to_string(), "asInt": "170864640", "attributes": []})
        );
    }

    #[test]
    fn cumulative_cpu_and_the_gauges() {
        let body = encoded(Temporality::Cumulative, None);
        let run = &body["resourceMetrics"][0];
        let cpu = metric(run, "deck.run.cpu.time");
        assert_eq!(cpu["unit"], "s");
        assert_eq!(cpu["sum"]["aggregationTemporality"], 2);
        assert_eq!(cpu["sum"]["isMonotonic"], true);
        assert_eq!(
            cpu["sum"]["dataPoints"][0],
            json!({
                "startTimeUnixNano": "1792890000000000000",
                "timeUnixNano": T.to_string(),
                "asDouble": 4.02,
                "attributes": [],
            })
        );
        let pss = metric(run, "deck.run.memory.pss");
        assert_eq!(pss["unit"], "By");
        let points = pss["gauge"]["dataPoints"].as_array().unwrap();
        let by_kind: Vec<_> = points
            .iter()
            .map(|p| {
                assert!(p.get("startTimeUnixNano").is_none());
                (
                    p["attributes"][0]["key"].as_str().unwrap(),
                    p["attributes"][0]["value"]["stringValue"].as_str().unwrap(),
                    p["asInt"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            by_kind,
            [
                ("deck.memory.kind", "anon", "502407168"),
                ("deck.memory.kind", "file", "226168832"),
                ("deck.memory.kind", "shmem", "0"),
            ]
        );
        let processes = metric(run, "deck.run.processes");
        assert_eq!(processes["unit"], "{process}");
        assert_eq!(processes["gauge"]["dataPoints"][0]["asInt"], "6");
    }

    #[test]
    fn delta_cpu_starts_at_the_previous_sample() {
        let body = encoded(Temporality::Delta, None);
        let cpu = metric(&body["resourceMetrics"][1], "deck.run.cpu.time");
        assert_eq!(cpu["sum"]["aggregationTemporality"], 1);
        let point = &cpu["sum"]["dataPoints"][0];
        assert_eq!(point["asDouble"], 0.25);
        assert_eq!(point["startTimeUnixNano"], (T - 15_000_000_000).to_string());
        // The first point of a series starts where the series does.
        let mut first = sample();
        first.runs[0].figures.previous_unix_nanos = None;
        let request = build_request(
            &first,
            &HashMap::new(),
            &[],
            &settings(Temporality::Delta),
            None,
        );
        let body: Value = serde_json::from_slice(&encode_json(&request)).unwrap();
        assert_eq!(
            metric(&body["resourceMetrics"][0], "deck.run.cpu.time")["sum"]["dataPoints"][0]["startTimeUnixNano"],
            "1792890000000000000"
        );
    }

    #[test]
    fn a_path_is_sent_only_when_given() {
        let raw =
            String::from_utf8(serde_json::to_vec(&encoded(Temporality::Cumulative, None)).unwrap())
                .unwrap();
        assert!(!raw.contains("deck.worktree.path"), "{raw}");
        let body = encoded(Temporality::Cumulative, Some("/home/u/code/repo"));
        for i in [0, 2] {
            assert!(
                attrs(&body["resourceMetrics"][i])
                    .contains(&("deck.worktree.path".into(), "/home/u/code/repo".into()))
            );
        }
    }

    #[test]
    fn without_a_host_or_runs_the_request_still_has_the_unattributed_bucket() {
        let empty = Sample {
            time_unix_nanos: T,
            runs: Vec::new(),
            unattributed: figures(0.0, 0.0, 0),
        };
        let request = build_request(
            &empty,
            &HashMap::new(),
            &[],
            &settings(Temporality::Cumulative),
            None,
        );
        assert_eq!(request.resources.len(), 1);
        let keys: Vec<_> = request.resources[0]
            .attributes
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert!(!keys.contains(&"host.name"));
        assert!(keys.contains(&"deck.attribution"));
        let body = encode_json(&request);
        assert!(!body.contains(&b'\n'), "one line per request");
    }

    /// Decode a protobuf body back into the OTLP/JSON shape, field by field,
    /// so the two encodings can be compared whole.
    fn protobuf_as_json(body: &[u8]) -> Value {
        enum F<'a> {
            Varint(u64),
            Fixed64(u64),
            Bytes(&'a [u8]),
        }
        fn varint(buf: &[u8], pos: &mut usize) -> u64 {
            let mut value = 0u64;
            let mut shift = 0;
            loop {
                let byte = buf[*pos];
                *pos += 1;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        }
        fn fields(buf: &[u8]) -> Vec<(u64, F<'_>)> {
            let mut out = Vec::new();
            let mut pos = 0;
            while pos < buf.len() {
                let tag = varint(buf, &mut pos);
                let value = match tag & 7 {
                    0 => F::Varint(varint(buf, &mut pos)),
                    1 => {
                        let v = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
                        pos += 8;
                        F::Fixed64(v)
                    }
                    2 => {
                        let len = varint(buf, &mut pos) as usize;
                        pos += len;
                        F::Bytes(&buf[pos - len..pos])
                    }
                    wire => panic!("unexpected wire type {wire}"),
                };
                out.push((tag >> 3, value));
            }
            out
        }
        fn text(bytes: &[u8]) -> String {
            String::from_utf8(bytes.to_vec()).unwrap()
        }
        fn attribute(kv: &[u8]) -> Value {
            let mut key = String::new();
            let mut value = String::new();
            for (n, f) in fields(kv) {
                match (n, f) {
                    (1, F::Bytes(b)) => key = text(b),
                    (2, F::Bytes(any)) => {
                        for (m, g) in fields(any) {
                            if let (1, F::Bytes(b)) = (m, g) {
                                value = text(b);
                            }
                        }
                    }
                    _ => panic!("unexpected KeyValue field {n}"),
                }
            }
            json!({ "key": key, "value": { "stringValue": value } })
        }
        fn point(buf: &[u8]) -> Value {
            let mut out = json!({ "attributes": [] });
            for (n, f) in fields(buf) {
                match (n, f) {
                    (2, F::Fixed64(v)) => out["startTimeUnixNano"] = json!(v.to_string()),
                    (3, F::Fixed64(v)) => out["timeUnixNano"] = json!(v.to_string()),
                    (4, F::Fixed64(v)) => out["asDouble"] = json!(f64::from_bits(v)),
                    (6, F::Fixed64(v)) => out["asInt"] = json!((v as i64).to_string()),
                    (7, F::Bytes(b)) => {
                        out["attributes"].as_array_mut().unwrap().push(attribute(b))
                    }
                    _ => panic!("unexpected NumberDataPoint field {n}"),
                }
            }
            out
        }
        fn metric(buf: &[u8]) -> Value {
            let mut out = json!({});
            for (n, f) in fields(buf) {
                match (n, f) {
                    (1, F::Bytes(b)) => out["name"] = json!(text(b)),
                    (2, F::Bytes(b)) => out["description"] = json!(text(b)),
                    (3, F::Bytes(b)) => out["unit"] = json!(text(b)),
                    (5, F::Bytes(b)) => {
                        let points: Vec<Value> = fields(b)
                            .into_iter()
                            .map(|(_, p)| match p {
                                F::Bytes(p) => point(p),
                                _ => panic!("gauge point"),
                            })
                            .collect();
                        out["gauge"] = json!({ "dataPoints": points });
                    }
                    (7, F::Bytes(b)) => {
                        let mut sum = json!({ "dataPoints": [], "isMonotonic": false });
                        for (m, g) in fields(b) {
                            match (m, g) {
                                (1, F::Bytes(p)) => {
                                    sum["dataPoints"].as_array_mut().unwrap().push(point(p))
                                }
                                (2, F::Varint(v)) => sum["aggregationTemporality"] = json!(v),
                                (3, F::Varint(v)) => sum["isMonotonic"] = json!(v == 1),
                                _ => panic!("unexpected Sum field {m}"),
                            }
                        }
                        out["sum"] = sum;
                    }
                    _ => panic!("unexpected Metric field {n}"),
                }
            }
            out
        }
        let mut resources = Vec::new();
        for (_, rm) in fields(body) {
            let F::Bytes(rm) = rm else {
                panic!("ResourceMetrics")
            };
            let mut out = json!({});
            for (n, f) in fields(rm) {
                let F::Bytes(b) = f else {
                    panic!("ResourceMetrics field")
                };
                match n {
                    1 => {
                        let attributes: Vec<Value> = fields(b)
                            .into_iter()
                            .map(|(_, kv)| match kv {
                                F::Bytes(kv) => attribute(kv),
                                _ => panic!("attribute"),
                            })
                            .collect();
                        out["resource"] = json!({ "attributes": attributes });
                    }
                    2 => {
                        let mut scope = json!({});
                        let mut metrics = Vec::new();
                        for (m, g) in fields(b) {
                            let F::Bytes(g) = g else {
                                panic!("ScopeMetrics field")
                            };
                            match m {
                                1 => {
                                    for (k, h) in fields(g) {
                                        let F::Bytes(h) = h else { panic!("scope") };
                                        let key = if k == 1 { "name" } else { "version" };
                                        scope[key] = json!(text(h));
                                    }
                                }
                                2 => metrics.push(metric(g)),
                                _ => panic!("unexpected ScopeMetrics field {m}"),
                            }
                        }
                        out["scopeMetrics"] = json!([{ "scope": scope, "metrics": metrics }]);
                    }
                    _ => panic!("unexpected ResourceMetrics field {n}"),
                }
            }
            resources.push(out);
        }
        json!({ "resourceMetrics": resources })
    }

    #[test]
    fn protobuf_and_json_describe_the_same_request() {
        for temporality in [Temporality::Cumulative, Temporality::Delta] {
            let worktrees = HashMap::from([(run_1(), worktree(Some("/w")))]);
            let disks = [WorktreeUsage {
                worktree: worktree(Some("/w")),
                bytes: 170_864_640,
                measured_unix_nanos: T - 1,
                complete: false,
            }];
            let request = build_request(
                &sample(),
                &worktrees,
                &disks,
                &settings(temporality),
                Some("studio"),
            );
            let json: Value = serde_json::from_slice(&encode_json(&request)).unwrap();
            assert_eq!(protobuf_as_json(&encode_protobuf(&request)), json);
        }
    }

    fn ended(mut sample: Sample) -> Sample {
        sample.runs[0].ended = true;
        sample.runs[0].members.clear();
        sample
    }

    /// R1: a run's worktree is fixed when it is first seen, survives its
    /// oldest process leaving or changing directory, and is still on its
    /// terminal resource, so live and terminal points share one identity.
    #[test]
    fn a_runs_worktree_is_sticky_through_its_terminal_report() {
        let mut runs = RunWorktrees::default();
        let first = WorktreeRef {
            id: "first".into(),
            path: None,
        };
        let other = WorktreeRef {
            id: "other".into(),
            path: None,
        };
        // Only pid 11 has a worktree at first.
        runs.update(&sample(), |pid| (pid == 11).then(|| first.clone()));
        assert_eq!(runs.get().get(&run_1()), Some(&first));
        // Pid 10 now has another worktree: the run keeps its own.
        runs.update(&sample(), |_| Some(other.clone()));
        assert_eq!(runs.get().get(&run_1()), Some(&first));

        let live = build_request(
            &sample(),
            runs.get(),
            &[],
            &settings(Temporality::Cumulative),
            None,
        );
        let last = ended(sample());
        runs.update(&last, |_| None);
        let terminal = build_request(
            &last,
            runs.get(),
            &[],
            &settings(Temporality::Cumulative),
            None,
        );
        assert_eq!(
            live.resources[0].attributes,
            terminal.resources[0].attributes
        );
        assert!(terminal.resources[0].terminal);
        assert!(!live.resources[0].terminal);
        assert!(
            terminal.resources[0]
                .attributes
                .contains(&("deck.worktree.id".into(), "first".into()))
        );
        runs.forget_ended(&last);
        assert!(runs.get().is_empty());
    }

    /// R3: an ended run's last report rides along until a request carrying
    /// it is delivered, and no more than the bound is held.
    #[test]
    fn a_terminal_report_is_held_until_delivered() {
        let settings = settings(Temporality::Cumulative);
        let mut pending = PendingTerminals::default();
        let mut failed = build_request(&ended(sample()), &HashMap::new(), &[], &settings, None);
        pending.attach(&mut failed);
        assert_eq!(failed.resources.len(), 2);
        assert_eq!(pending.failed(&failed), 0);
        assert_eq!(pending.len(), 1);

        // The next sample no longer has the run; its request carries it.
        let mut next_sample = sample();
        next_sample.runs.clear();
        let mut next = build_request(&next_sample, &HashMap::new(), &[], &settings, None);
        pending.attach(&mut next);
        assert_eq!(next.resources.len(), 2);
        assert_eq!(next.resources[1], failed.resources[0]);
        // Failing again holds it once, not twice.
        pending.failed(&next);
        assert_eq!(pending.len(), 1);
        pending.delivered();
        assert!(pending.is_empty());

        let mut many = build_request(&next_sample, &HashMap::new(), &[], &settings, None);
        many.resources.extend(std::iter::repeat_n(
            failed.resources[0].clone(),
            MAX_PENDING_TERMINALS + 3,
        ));
        assert_eq!(pending.failed(&many), 3);
        assert_eq!(pending.len(), MAX_PENDING_TERMINALS);
        assert_eq!(pending.dropped, 3);
    }

    /// Finding 7: the sample in progress at shutdown is not sent, and its
    /// ended runs' last reports go with the final request instead.
    #[test]
    fn a_superseded_request_hands_its_terminal_reports_on() {
        let settings = settings(Temporality::Cumulative);
        let mut pending = PendingTerminals::default();
        let superseded = build_request(&ended(sample()), &HashMap::new(), &[], &settings, None);
        assert_eq!(pending.hold(&superseded), 0);
        assert_eq!(pending.len(), 1);
        let mut final_sample = sample();
        final_sample.runs.clear();
        let mut last = build_request(&final_sample, &HashMap::new(), &[], &settings, None);
        pending.attach(&mut last);
        assert_eq!(last.resources.last(), Some(&superseded.resources[0]));
        assert!(last.resources.last().unwrap().terminal);
    }

    /// R1/U5: a run first seen in no worktree keeps none, even when it
    /// enters one later, so its live and terminal points share one identity.
    #[test]
    fn a_run_first_seen_in_no_worktree_keeps_none() {
        let settings = settings(Temporality::Cumulative);
        let mut runs = RunWorktrees::default();
        runs.update(&sample(), |_| None);
        let before = build_request(&sample(), runs.get(), &[], &settings, None);
        // It enters a worktree.
        runs.update(&sample(), |_| Some(worktree(None)));
        let after = build_request(&sample(), runs.get(), &[], &settings, None);
        let last = ended(sample());
        runs.update(&last, |_| Some(worktree(None)));
        let terminal = build_request(&last, runs.get(), &[], &settings, None);
        assert!(terminal.resources[0].terminal);
        for request in [&after, &terminal] {
            assert_eq!(
                request.resources[0].attributes,
                before.resources[0].attributes
            );
        }
        assert!(
            !before.resources[0]
                .attributes
                .iter()
                .any(|(k, _)| k == "deck.worktree.id")
        );
        // Once the run has ended, a new run with its key is decided afresh.
        runs.forget_ended(&last);
        runs.update(&sample(), |_| Some(worktree(None)));
        assert_eq!(runs.get().get(&run_1()), Some(&worktree(None)));
    }

    /// U4: past the bound, the oldest held terminal report is dropped, not
    /// the one that just ended, across several failed requests.
    #[test]
    fn held_terminals_overflow_drops_the_oldest() {
        let settings = settings(Temporality::Cumulative);
        let template = build_request(&ended(sample()), &HashMap::new(), &[], &settings, None)
            .resources[0]
            .clone();
        let terminal = |name: &str| {
            let mut resource = template.clone();
            for (key, value) in &mut resource.attributes {
                if key == "deck.run.id" {
                    *value = name.to_string();
                }
            }
            resource
        };
        let run_ids = |request: &Request| -> Vec<String> {
            request
                .resources
                .iter()
                .filter(|r| r.terminal)
                .filter_map(|r| {
                    r.attributes
                        .iter()
                        .find(|(k, _)| k == "deck.run.id")
                        .map(|(_, v)| v.clone())
                })
                .collect()
        };
        let mut no_runs = sample();
        no_runs.runs.clear();
        let empty = || build_request(&no_runs, &HashMap::new(), &[], &settings, None);

        let mut pending = PendingTerminals::default();
        let mut first = empty();
        first
            .resources
            .extend((0..MAX_PENDING_TERMINALS).map(|i| terminal(&format!("old-{i}"))));
        pending.attach(&mut first);
        assert_eq!(pending.failed(&first), 0);
        assert_eq!(pending.len(), MAX_PENDING_TERMINALS);

        for name in ["new-1", "new-2"] {
            let mut next = empty();
            next.resources.push(terminal(name));
            pending.attach(&mut next);
            assert_eq!(pending.failed(&next), 1);
        }
        assert_eq!(pending.dropped, 2);
        let mut last = empty();
        pending.attach(&mut last);
        let held = run_ids(&last);
        assert_eq!(held.len(), MAX_PENDING_TERMINALS);
        assert_eq!(held.first().map(String::as_str), Some("old-2"));
        assert_eq!(&held[held.len() - 2..], ["new-1", "new-2"]);
        assert!(!held.iter().any(|id| id == "old-0" || id == "old-1"));
    }

    /// A2: a walk that stopped at a limit says so on its point; with
    /// `--all-users`, a run names its user.
    #[test]
    fn incomplete_walks_and_run_users_are_labelled() {
        let mut settings = settings(Temporality::Cumulative);
        settings.label_uids = true;
        let disks = [WorktreeUsage {
            worktree: worktree(None),
            bytes: 1,
            measured_unix_nanos: T,
            complete: false,
        }];
        let request = build_request(&sample(), &HashMap::new(), &disks, &settings, None);
        assert!(
            request.resources[0]
                .attributes
                .contains(&("deck.run.uid".into(), "1000".into()))
        );
        let MetricData::Gauge { points } = &request.resources[2].metrics[0].data else {
            panic!("a gauge");
        };
        assert_eq!(
            points[0].attributes,
            vec![("deck.worktree.disk.complete".into(), "false".into())]
        );
        assert_eq!(request.data_points(), 5 + 5 + 1);
    }
}
