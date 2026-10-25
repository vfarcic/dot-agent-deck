//! OTLP encoding of an `ExportTraceServiceRequest`: protobuf for
//! `http/protobuf`, the OTLP/JSON mapping for `http/json`; and decoding of the
//! one part of the `ExportTraceServiceResponse` the deck reads, its
//! `partial_success`.
//!
//! Only the fields the deck fills are written. Field numbers are those of
//! `opentelemetry/proto/trace/v1/trace.proto`, `common/v1/common.proto`,
//! `resource/v1/resource.proto` and `collector/trace/v1/trace_service.proto`.

use super::resource::ResourceAttribute;
use super::span::{AttrValue, SpanEventRecord, SpanRecord, hex};

/// `SpanKind.SPAN_KIND_INTERNAL`: the deck's spans describe work it observed,
/// not a request it served or made.
const SPAN_KIND_INTERNAL: u64 = 1;

const SCOPE_NAME: &str = "dot-agent-deck";

const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LEN: u8 = 2;
const WIRE_FIXED32: u8 = 5;

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

pub(crate) fn put_bytes(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_tag(buf, field, WIRE_LEN);
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

pub(crate) fn put_fixed64(buf: &mut Vec<u8>, field: u32, value: u64) {
    put_tag(buf, field, WIRE_FIXED64);
    buf.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_varint_field(buf: &mut Vec<u8>, field: u32, value: u64) {
    put_tag(buf, field, WIRE_VARINT);
    put_varint(buf, value);
}

fn any_value(value: &AttrValue) -> Vec<u8> {
    let mut buf = Vec::new();
    match value {
        AttrValue::Str(s) => put_bytes(&mut buf, 1, s.as_bytes()),
        AttrValue::Bool(b) => put_varint_field(&mut buf, 2, u64::from(*b)),
        AttrValue::Int(i) => put_varint_field(&mut buf, 3, *i as u64),
    }
    buf
}

fn key_value(key: &str, value: &AttrValue) -> Vec<u8> {
    let mut buf = Vec::new();
    put_bytes(&mut buf, 1, key.as_bytes());
    put_bytes(&mut buf, 2, &any_value(value));
    buf
}

fn event_message(event: &SpanEventRecord) -> Vec<u8> {
    let mut buf = Vec::new();
    put_fixed64(&mut buf, 1, event.time_unix_nanos);
    put_bytes(&mut buf, 2, event.name.as_bytes());
    for attr in &event.attributes {
        put_bytes(&mut buf, 3, &key_value(attr.key.as_str(), &attr.value));
    }
    buf
}

fn span_message(span: &SpanRecord) -> Vec<u8> {
    let mut buf = Vec::new();
    put_bytes(&mut buf, 1, &span.trace_id);
    put_bytes(&mut buf, 2, &span.span_id);
    if let Some(parent) = &span.parent_span_id {
        put_bytes(&mut buf, 4, parent);
    }
    put_bytes(&mut buf, 5, span.name.as_bytes());
    put_varint_field(&mut buf, 6, SPAN_KIND_INTERNAL);
    put_fixed64(&mut buf, 7, span.start_unix_nanos);
    put_fixed64(&mut buf, 8, span.end_unix_nanos);
    for attr in &span.attributes {
        put_bytes(&mut buf, 9, &key_value(attr.key.as_str(), &attr.value));
    }
    for event in &span.events {
        put_bytes(&mut buf, 11, &event_message(event));
    }
    buf
}

/// The protobuf body for `http/protobuf`.
pub fn encode_protobuf(resource: &[ResourceAttribute], spans: &[SpanRecord]) -> Vec<u8> {
    let mut resource_msg = Vec::new();
    for attr in resource {
        put_bytes(&mut resource_msg, 1, &key_value(attr.key(), &attr.value()));
    }
    let mut scope = Vec::new();
    put_bytes(&mut scope, 1, SCOPE_NAME.as_bytes());
    put_bytes(&mut scope, 2, env!("DAD_VERSION").as_bytes());
    let mut scope_spans = Vec::new();
    put_bytes(&mut scope_spans, 1, &scope);
    for span in spans {
        put_bytes(&mut scope_spans, 2, &span_message(span));
    }
    let mut resource_spans = Vec::new();
    put_bytes(&mut resource_spans, 1, &resource_msg);
    put_bytes(&mut resource_spans, 2, &scope_spans);
    let mut request = Vec::new();
    put_bytes(&mut request, 1, &resource_spans);
    request
}

fn json_value(value: &AttrValue) -> serde_json::Value {
    match value {
        AttrValue::Str(s) => serde_json::json!({ "stringValue": s }),
        AttrValue::Bool(b) => serde_json::json!({ "boolValue": b }),
        // OTLP/JSON writes 64-bit integers as decimal strings.
        AttrValue::Int(i) => serde_json::json!({ "intValue": i.to_string() }),
    }
}

fn json_kv(key: &str, value: &AttrValue) -> serde_json::Value {
    serde_json::json!({ "key": key, "value": json_value(value) })
}

/// The OTLP/JSON body for `http/json`. Ids are lowercase hex, as the OTLP/JSON
/// mapping requires, not the base64 a generic protobuf-JSON mapping would use.
pub fn encode_json(resource: &[ResourceAttribute], spans: &[SpanRecord]) -> Vec<u8> {
    let spans: Vec<serde_json::Value> = spans
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
                    .map(|a| json_kv(a.key.as_str(), &a.value))
                    .collect::<Vec<_>>(),
                "events": span.events.iter().map(|e| serde_json::json!({
                    "timeUnixNano": e.time_unix_nanos.to_string(),
                    "name": e.name,
                    "attributes": e.attributes.iter()
                        .map(|a| json_kv(a.key.as_str(), &a.value))
                        .collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            });
            if let Some(parent) = &span.parent_span_id {
                value["parentSpanId"] = serde_json::Value::String(hex(parent));
            }
            value
        })
        .collect();
    let body = serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": resource.iter()
                    .map(|a| json_kv(a.key(), &a.value()))
                    .collect::<Vec<_>>(),
            },
            "scopeSpans": [{
                "scope": { "name": SCOPE_NAME, "version": env!("DAD_VERSION") },
                "spans": spans,
            }],
        }],
    });
    serde_json::to_vec(&body).expect("a JSON value always serializes")
}

/// The `partial_success` of an `ExportTraceServiceResponse`: spans the
/// backend did not accept, and its message about them (or a warning, with
/// none rejected).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialSuccess {
    pub rejected_spans: i64,
    pub error_message: String,
}

/// One field of a protobuf message, as far as a response is read.
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The fields of a protobuf message, by number; `None` when it is malformed.
fn read_fields(buf: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    let skip = |pos: &mut usize, n: usize| {
        *pos = pos.checked_add(n).filter(|end| *end <= buf.len())?;
        Some(Wire::Other)
    };
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let value = match (tag & 7) as u8 {
            WIRE_VARINT => Wire::Varint(read_varint(buf, &mut pos)?),
            WIRE_FIXED64 => skip(&mut pos, 8)?,
            WIRE_FIXED32 => skip(&mut pos, 4)?,
            WIRE_LEN => {
                let len = usize::try_from(read_varint(buf, &mut pos)?).ok()?;
                let end = pos.checked_add(len).filter(|end| *end <= buf.len())?;
                let bytes = &buf[pos..end];
                pos = end;
                Wire::Bytes(bytes)
            }
            _ => return None,
        };
        out.push((tag >> 3, value));
    }
    Some(out)
}

/// The `partial_success` (field 1) of a protobuf `ExportTraceServiceResponse`;
/// `None` when the body has none or is not such a message.
pub fn decode_partial_success_protobuf(body: &[u8]) -> Option<PartialSuccess> {
    let partial = read_fields(body)?
        .into_iter()
        .filter_map(|(field, value)| match (field, value) {
            (1, Wire::Bytes(bytes)) => Some(bytes),
            _ => None,
        })
        .next_back()?;
    let mut out = PartialSuccess::default();
    for (field, value) in read_fields(partial)? {
        match (field, value) {
            (1, Wire::Varint(rejected)) => out.rejected_spans = rejected as i64,
            (2, Wire::Bytes(message)) => {
                out.error_message = String::from_utf8_lossy(message).into_owned();
            }
            _ => {}
        }
    }
    Some(out)
}

/// The `partialSuccess` of an OTLP/JSON `ExportTraceServiceResponse`, under
/// either spelling the JSON mapping allows; `None` when the body has none or is
/// not JSON. The count may be a string, as the mapping writes an `int64`, or a
/// number.
pub fn decode_partial_success_json(body: &[u8]) -> Option<PartialSuccess> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let field = |v: &'_ serde_json::Value, camel: &str, snake: &str| {
        v.get(camel).or_else(|| v.get(snake)).cloned()
    };
    let partial = field(&value, "partialSuccess", "partial_success")?;
    let rejected_spans = match field(&partial, "rejectedSpans", "rejected_spans") {
        Some(serde_json::Value::String(count)) => count.trim().parse().unwrap_or(0),
        Some(serde_json::Value::Number(count)) => count.as_i64().unwrap_or(0),
        _ => 0,
    };
    let error_message = match field(&partial, "errorMessage", "error_message") {
        Some(serde_json::Value::String(message)) => message,
        _ => String::new(),
    };
    Some(PartialSuccess {
        rejected_spans,
        error_message,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::event::{AgentType, EventType};
    use crate::telemetry::config::{EnvMap, TelemetryConfig};
    use crate::telemetry::resource::deck_resource;
    use crate::telemetry::run_context::RunContext;
    use crate::telemetry::span::Attribute;
    use crate::telemetry::tracker::tests::{event, start};
    use crate::telemetry::tracker::{EndReason, OrchestrationInfo, RunTracker};
    use crate::telemetry::{AttrKey, SPAN_AGENT_RUN};

    // ---- a minimal, schema-aware protobuf reader for the assertions ----

    #[derive(Debug)]
    enum Field<'a> {
        Varint(u64),
        Fixed64(u64),
        Bytes(&'a [u8]),
    }

    fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
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

    fn fields(buf: &[u8]) -> Vec<(u32, Field<'_>)> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < buf.len() {
            let tag = read_varint(buf, &mut pos);
            let field = (tag >> 3) as u32;
            let value = match (tag & 7) as u8 {
                WIRE_VARINT => Field::Varint(read_varint(buf, &mut pos)),
                WIRE_FIXED64 => {
                    let v = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
                    pos += 8;
                    Field::Fixed64(v)
                }
                WIRE_LEN => {
                    let len = read_varint(buf, &mut pos) as usize;
                    let v = &buf[pos..pos + len];
                    pos += len;
                    Field::Bytes(v)
                }
                other => panic!("unexpected wire type {other}"),
            };
            out.push((field, value));
        }
        assert_eq!(pos, buf.len(), "message ends exactly at its length");
        out
    }

    fn sub(buf: &[u8], field: u32) -> Vec<&[u8]> {
        fields(buf)
            .into_iter()
            .filter_map(|(f, v)| match v {
                Field::Bytes(b) if f == field => Some(b),
                _ => None,
            })
            .collect()
    }

    fn string(buf: &[u8], field: u32) -> String {
        String::from_utf8(sub(buf, field)[0].to_vec()).unwrap()
    }

    fn kv_keys(container: &[u8], field: u32) -> Vec<String> {
        sub(container, field)
            .into_iter()
            .map(|kv| string(kv, 1))
            .collect()
    }

    /// Every attribute key in a protobuf request: (resource keys, span and
    /// span-event keys), plus the span names.
    fn protobuf_keys(body: &[u8]) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut resource_keys = Vec::new();
        let mut span_keys = Vec::new();
        let mut names = Vec::new();
        for rs in sub(body, 1) {
            for resource in sub(rs, 1) {
                resource_keys.extend(kv_keys(resource, 1));
            }
            for ss in sub(rs, 2) {
                for span in sub(ss, 2) {
                    names.push(string(span, 5));
                    span_keys.extend(kv_keys(span, 9));
                    for event in sub(span, 11) {
                        span_keys.extend(kv_keys(event, 3));
                    }
                }
            }
        }
        (resource_keys, span_keys, names)
    }

    /// The values of the attributes in `container`'s `field`, as text: a
    /// string as itself, a bool or an int in decimal.
    fn kv_values(container: &[u8], field: u32) -> Vec<String> {
        sub(container, field)
            .into_iter()
            .flat_map(|kv| sub(kv, 2))
            .map(|any| match fields(any).into_iter().next() {
                Some((1, Field::Bytes(b))) => String::from_utf8(b.to_vec()).unwrap(),
                Some((2, Field::Varint(v))) => (v != 0).to_string(),
                Some((3, Field::Varint(v))) => (v as i64).to_string(),
                other => panic!("unexpected AnyValue {other:?}"),
            })
            .collect()
    }

    /// Every attribute value in a protobuf request — resource, span and span
    /// event — as text, decoded rather than searched for in the bytes.
    pub(crate) fn protobuf_values(body: &[u8]) -> Vec<String> {
        let mut values = Vec::new();
        for rs in sub(body, 1) {
            for resource in sub(rs, 1) {
                values.extend(kv_values(resource, 1));
            }
            for ss in sub(rs, 2) {
                for span in sub(ss, 2) {
                    values.extend(kv_values(span, 9));
                    for event in sub(span, 11) {
                        values.extend(kv_values(event, 3));
                    }
                }
            }
        }
        values
    }

    /// The same for an OTLP/JSON request.
    pub(crate) fn json_values(body: &[u8]) -> Vec<String> {
        fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, v) in map {
                        match (k.as_str(), v) {
                            ("stringValue" | "intValue", serde_json::Value::String(s)) => {
                                out.push(s.clone());
                            }
                            ("boolValue", serde_json::Value::Bool(b)) => out.push(b.to_string()),
                            _ => walk(v, out),
                        }
                    }
                }
                serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        let mut values = Vec::new();
        walk(&serde_json::from_slice(body).unwrap(), &mut values);
        values
    }

    fn json_keys(body: &[u8]) -> (Vec<String>, Vec<String>, Vec<String>) {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        let keys = |attrs: &serde_json::Value| -> Vec<String> {
            attrs
                .as_array()
                .unwrap()
                .iter()
                .map(|kv| kv["key"].as_str().unwrap().to_string())
                .collect()
        };
        let rs = &v["resourceSpans"][0];
        let resource_keys = keys(&rs["resource"]["attributes"]);
        let mut span_keys = Vec::new();
        let mut names = Vec::new();
        for span in rs["scopeSpans"][0]["spans"].as_array().unwrap() {
            names.push(span["name"].as_str().unwrap().to_string());
            span_keys.extend(keys(&span["attributes"]));
            for event in span["events"].as_array().unwrap() {
                span_keys.extend(keys(&event["attributes"]));
            }
        }
        (resource_keys, span_keys, names)
    }

    #[test]
    fn varint_and_fixed64_encodings() {
        let mut buf = Vec::new();
        put_varint(&mut buf, 300);
        assert_eq!(buf, [0xac, 0x02]);
        let mut buf = Vec::new();
        put_varint_field(&mut buf, 3, -1i64 as u64);
        assert_eq!(buf.len(), 11, "a negative int64 is a ten-byte varint");
        let mut buf = Vec::new();
        put_fixed64(&mut buf, 7, 1);
        assert_eq!(buf, [0x39, 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn protobuf_and_json_describe_the_same_spans() {
        let mut tracker = RunTracker::new();
        tracker.begin_run(start("p1"), 10);
        tracker.observe_event(&event("p1", EventType::ToolStart), 20);
        tracker.observe_human_input("p1", 25);
        tracker.end_run("p1", EndReason::Exited, Some(-1), 30);
        let spans = tracker.take_finished();
        let resource = deck_resource("dot-agent-deck", Some("host-a"), &[]);

        let proto = encode_protobuf(&resource, &spans);
        let json = encode_json(&resource, &spans);
        assert_eq!(protobuf_keys(&proto), json_keys(&json));

        // Ids, parentage and timestamps survive both encodings.
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let json_spans = v["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        let proto_spans = sub(sub(sub(&proto, 1)[0], 2)[0], 2);
        assert_eq!(json_spans.len(), proto_spans.len());
        for (j, p) in json_spans.iter().zip(proto_spans) {
            let f = fields(p);
            let bytes_of = |n: u32| {
                f.iter().find_map(|(field, v)| match v {
                    Field::Bytes(b) if *field == n => Some(hex(b)),
                    _ => None,
                })
            };
            let fixed = |n: u32| {
                f.iter().find_map(|(field, v)| match v {
                    Field::Fixed64(x) if *field == n => Some(x.to_string()),
                    _ => None,
                })
            };
            assert_eq!(bytes_of(1).as_deref(), j["traceId"].as_str());
            assert_eq!(bytes_of(2).as_deref(), j["spanId"].as_str());
            assert_eq!(bytes_of(4).as_deref(), j["parentSpanId"].as_str());
            assert_eq!(fixed(7).as_deref(), j["startTimeUnixNano"].as_str());
            assert_eq!(fixed(8).as_deref(), j["endTimeUnixNano"].as_str());
            assert!(
                f.iter()
                    .any(|(n, v)| *n == 6 && matches!(v, Field::Varint(1)))
            );
            assert_eq!(j["traceId"].as_str().unwrap().len(), 32);
        }
        assert!(json_spans.iter().any(|s| s["name"] == SPAN_AGENT_RUN));
        let exit = json_spans
            .iter()
            .flat_map(|s| s["attributes"].as_array().unwrap())
            .find(|kv| kv["key"] == AttrKey::ExitCode.as_str())
            .unwrap();
        assert_eq!(exit["value"]["intValue"], "-1");
    }

    // ---- the never-sent rule ----

    /// Strings planted in every content-bearing place a run's input has. None
    /// may appear in what the deck sends.
    pub(crate) const SENTINELS: [&str; 16] = [
        "SENTINEL-PROMPT",
        "SENTINEL-TOOL-NAME",
        "SENTINEL-TOOL-ARGS",
        "SENTINEL-CWD",
        "SENTINEL-METADATA-VALUE",
        "SENTINEL-METADATA-KEY",
        "SENTINEL-TITLE",
        "SENTINEL-HEADER-VALUE",
        "SENTINEL-ENV-VALUE",
        "SENTINEL-SESSION",
        "SENTINEL-AGENT-ID",
        "SENTINEL-AGENT-VERSION",
        "SENTINEL-PR-URL",
        "SENTINEL-BRANCH",
        "SENTINEL-COMMAND",
        "SENTINEL-INPUT",
    ];

    /// The generation id the sentinel run and its events carry: a string the
    /// tracker matches on and never sends.
    pub(crate) const SENTINEL_AGENT_ID: &str = "SENTINEL-AGENT-ID";

    /// An event with a sentinel in every string field `AgentEvent` has —
    /// session and generation ids, tool name and detail, cwd, prompt, agent
    /// version, and metadata keys and values, including the daemon's own
    /// display-name, status-detail and branch keys. The remaining fields are
    /// enums, numbers and timestamps.
    pub(crate) fn loaded_event(pane: &str, event_type: EventType) -> crate::event::AgentEvent {
        let mut e = event(pane, event_type);
        e.session_id = "SENTINEL-SESSION".into();
        e.agent_id = Some(SENTINEL_AGENT_ID.into());
        e.tool_name = Some("SENTINEL-TOOL-NAME".into());
        e.tool_detail = Some("SENTINEL-TOOL-ARGS output: SENTINEL-PROMPT".into());
        e.cwd = Some("/home/u/SENTINEL-CWD".into());
        e.user_prompt = Some("please SENTINEL-PROMPT".into());
        e.agent_version = Some("SENTINEL-AGENT-VERSION".into());
        for key in [
            crate::event::DISPLAY_NAME_METADATA_KEY,
            "reason",
            "message",
            "last_assistant_message",
            "bash_command",
            "branch",
            "SENTINEL-METADATA-KEY",
        ] {
            e.metadata.insert(
                key.to_string(),
                "SENTINEL-METADATA-VALUE SENTINEL-TITLE SENTINEL-BRANCH SENTINEL-COMMAND".into(),
            );
        }
        if e.event_type == EventType::PullRequest {
            // A VALID report, so the path that reads one runs: its number is
            // sent, its URL is not.
            e.metadata.insert(
                crate::event::PULL_REQUEST_METADATA_KEY.into(),
                r#"{"number":4242,"url":"https://SENTINEL-PR-URL.example/o/r/pull/4242","state":"open"}"#
                    .into(),
            );
            e.metadata.insert(
                crate::event::PULL_REQUEST_REVISION_METADATA_KEY.into(),
                "1".into(),
            );
        }
        e
    }

    fn tracked_spans_with_sentinels() -> Vec<SpanRecord> {
        let mut tracker = RunTracker::new();
        let mut s = start("pane-1");
        s.agent_id = Some(SENTINEL_AGENT_ID.into());
        s.orchestration = Some(OrchestrationInfo {
            role: Some("coder".into()),
            name: Some("tdd".into()),
            id: Some("orch-1".into()),
            orchestrator: false,
        });
        tracker.begin_run(s, 1);
        tracker.begin_run(start("pane-2"), 1);
        tracker.set_context(
            "run-pane-1",
            RunContext {
                instructions_commit: Some("b".repeat(40)),
                repository_name: Some("repo".into()),
            },
        );
        let mut t = 2;
        for event_type in [
            EventType::SessionStart,
            EventType::ToolStart,
            EventType::Thinking,
            EventType::PermissionRequest,
            EventType::WaitingForInput,
            EventType::QuotaBlocked,
            EventType::Error,
            EventType::ToolEnd,
            EventType::ShellBusy,
            EventType::ShellIdle,
            EventType::Idle,
            EventType::PullRequest,
        ] {
            tracker.observe_event(&loaded_event("pane-1", event_type), t);
            t += 1;
        }
        // Shell commands, whose classification is the one thing a span reads
        // from an event's text: every sentinel is in a place the filter or
        // the deny-list keeps out.
        for command in [
            "SENTINEL-COMMAND --flag",
            "git commit -m 'SENTINEL-PROMPT'",
            "cat SENTINEL-TOOL-ARGS",
            "grep -r sentinel-input .",
            "cd /home/u/SENTINEL-CWD && ./SENTINEL-TITLE.sh",
            "TOKEN=SENTINEL-ENV-VALUE curl https://SENTINEL-PR-URL.example",
            "sudo -u root echo SENTINEL-BRANCH",
            // The later commands of a chain and a wrapper's command are read
            // by the same rules.
            "cargo fmt && git commit -m 'SENTINEL-PROMPT' && echo SENTINEL-BRANCH",
            "make ci && SENTINEL-COMMAND run",
            "timeout 5 cat SENTINEL-TOOL-ARGS",
            "kubectl exec SENTINEL-TITLE -- make ci",
        ] {
            for event_type in [EventType::ToolStart, EventType::ToolEnd] {
                let mut e = loaded_event("pane-1", event_type);
                e.tool_name = Some("Bash".into());
                e.tool_detail = Some(command.into());
                tracker.observe_event(&e, t);
                t += 1;
            }
        }
        tracker.observe_human_input("pane-1", t);
        tracker.end_run("pane-1", EndReason::Exited, Some(1), t + 1);
        tracker.end_all(EndReason::DaemonShutdown, t + 2);
        let spans = tracker.take_finished();
        let run = spans
            .iter()
            .find(|s| {
                s.name == SPAN_AGENT_RUN
                    && s.attributes
                        .contains(&Attribute::str(AttrKey::PaneId, "pane-1"))
            })
            .expect("pane-1's run");
        assert!(
            run.attributes
                .contains(&Attribute::int(AttrKey::PrNumber, 4242)),
            "the sentinel events were applied, the valid report included"
        );
        assert!(
            run.attributes
                .contains(&Attribute::int(AttrKey::OperationsCount, 11)),
            "the shell commands were classified"
        );
        spans
    }

    fn sentinel_config() -> TelemetryConfig {
        let env: EnvMap = [
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                "authorization=SENTINEL-HEADER-VALUE",
            ),
            ("SOME_TOKEN", "SENTINEL-ENV-VALUE"),
            ("OTEL_RESOURCE_ATTRIBUTES", "team=platform"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        TelemetryConfig::from_env(&env).unwrap()
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    #[test]
    fn no_content_reaches_either_payload() {
        let spans = tracked_spans_with_sentinels();
        assert!(spans.len() > 4, "the run produced status intervals");
        let config = sentinel_config();
        let resource = deck_resource(
            &config.service_name,
            Some("host-a"),
            &config.user_resource_attributes,
        );
        for body in [
            encode_protobuf(&resource, &spans),
            encode_json(&resource, &spans),
        ] {
            for sentinel in SENTINELS {
                assert!(
                    !contains(&body, sentinel),
                    "{sentinel} leaked into the payload: {}",
                    String::from_utf8_lossy(&body)
                );
            }
        }
    }

    #[test]
    fn every_emitted_key_is_allow_listed() {
        let spans = tracked_spans_with_sentinels();
        let config = sentinel_config();
        let resource = deck_resource(
            &config.service_name,
            Some("host-a"),
            &config.user_resource_attributes,
        );
        let user_keys: Vec<&str> = config
            .user_resource_attributes
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        for (resource_keys, span_keys, names) in [
            protobuf_keys(&encode_protobuf(&resource, &spans)),
            json_keys(&encode_json(&resource, &spans)),
        ] {
            assert!(!span_keys.is_empty());
            for key in &span_keys {
                assert!(
                    AttrKey::is_allowed(key),
                    "span key {key} is not allow-listed"
                );
            }
            for key in &resource_keys {
                assert!(
                    AttrKey::is_allowed(key) || user_keys.contains(&key.as_str()),
                    "resource key {key} is neither allow-listed nor the user's"
                );
            }
            for name in names {
                assert!(
                    crate::telemetry::SPAN_NAMES.contains(&name.as_str()),
                    "{name}"
                );
            }
        }
        // Each span carries each key at most once.
        for span in &spans {
            let mut seen = std::collections::HashSet::new();
            for Attribute { key, .. } in &span.attributes {
                assert!(seen.insert(*key), "{key:?} twice on {}", span.name);
            }
        }
        let _ = AgentType::None;
    }

    /// A response's `partial_success`, in both encodings: the count and the
    /// message read back, a missing field read as none, and a body that is not
    /// a response read as no report rather than a failure.
    #[test]
    fn partial_success_decodes_from_either_encoding() {
        let mut partial = Vec::new();
        put_varint_field(&mut partial, 1, 3);
        put_bytes(&mut partial, 2, b"bad span");
        let mut response = Vec::new();
        put_fixed64(&mut response, 9, 7);
        put_bytes(&mut response, 1, &partial);
        assert_eq!(
            decode_partial_success_protobuf(&response),
            Some(PartialSuccess {
                rejected_spans: 3,
                error_message: "bad span".into()
            })
        );
        assert_eq!(decode_partial_success_protobuf(&[]), None);
        assert_eq!(decode_partial_success_protobuf(&[0x0a, 0x05, 0x08]), None);
        assert_eq!(decode_partial_success_protobuf(&[0xff; 12]), None);

        for (body, expected) in [
            (
                r#"{"partialSuccess":{"rejectedSpans":"2","errorMessage":"too old"}}"#,
                Some((2, "too old")),
            ),
            (r#"{"partial_success":{"rejected_spans":4}}"#, Some((4, ""))),
            (r#"{"partialSuccess":{}}"#, Some((0, ""))),
            ("{}", None),
            ("", None),
            ("not json", None),
        ] {
            assert_eq!(
                decode_partial_success_json(body.as_bytes()),
                expected.map(|(rejected_spans, message)| PartialSuccess {
                    rejected_spans,
                    error_message: message.into()
                }),
                "{body}"
            );
        }
    }
}
