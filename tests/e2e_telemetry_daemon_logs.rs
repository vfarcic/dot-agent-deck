#![cfg(all(feature = "e2e", unix))]

//! Synthetic coverage for the daemon's warnings over OTLP logs (#1981 M8).
//! No agent is started; the warning comes from the real daemon and the logs
//! through the real export path.

mod common;
#[allow(dead_code)] // This target reads the receiver's raw bodies.
mod telemetry_operation_receiver;

use common::TuiDeck;
use serde_json::Value;
use spec::spec;
use std::io::Write;
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver};

fn launch(receiver: &Receiver) -> TuiDeck {
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_pty_size(180, 45)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    deck
}

/// Every log record the receiver got.
fn log_records(receiver: &Receiver) -> Vec<Value> {
    receiver
        .bodies()
        .iter()
        .filter_map(|body| serde_json::from_slice::<Value>(body).ok())
        .flat_map(|body| {
            let mut records = Vec::new();
            for resource in body["resourceLogs"].as_array().into_iter().flatten() {
                for scope in resource["scopeLogs"].as_array().into_iter().flatten() {
                    records.extend(
                        scope["logRecords"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .cloned(),
                    );
                }
            }
            records
        })
        .collect()
}

fn attr<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)?["value"]["stringValue"]
        .as_str()
}

/// Scenario: Start a deck whose daemon exports OTLP/JSON, then send its attach socket a frame longer than the protocol allows, which the daemon logs as a warning. The receiver must get an OTLP log record for that warning naming its severity, target, source file and field names but not the message text, and the daemon's log file must still record the warning as before.
#[spec("telemetry/otlp/044")]
#[test]
fn telemetry_otlp_044_daemon_warnings_are_exported_as_logs_without_their_text() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let mut stream = std::os::unix::net::UnixStream::connect(deck.attach_socket_path())
        .expect("connect to the attach socket");
    let mut header = [0u8; 5];
    header[0] = dot_agent_deck::daemon_protocol::KIND_REQ;
    header[1..].copy_from_slice(&u32::MAX.to_be_bytes());
    stream.write_all(&header).expect("write an oversized frame");
    let is_ours = |record: &Value| {
        attr(record, "deck.log.target") == Some("dot_agent_deck::daemon_protocol")
            && attr(record, "code.file.path") == Some("src/daemon_protocol.rs")
    };
    assert!(
        common::wait_until(EXPORT_WAIT, || log_records(&receiver).iter().any(is_ours)),
        "the daemon's warning never arrived as an OTLP log; records={:?}",
        log_records(&receiver)
    );
    let record = log_records(&receiver).into_iter().find(is_ours).unwrap();
    assert_eq!(record["severityText"], "WARN", "{record}");
    assert_eq!(record["severityNumber"], 13, "{record}");
    assert_eq!(
        attr(&record, "deck.log.fields"),
        Some("message"),
        "{record}"
    );
    assert!(
        record["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "code.line.number")
    );
    assert!(record.get("body").is_none(), "{record}");
    let resource_names: Vec<String> = receiver
        .bodies()
        .iter()
        .filter_map(|body| serde_json::from_slice::<Value>(body).ok())
        .flat_map(|body| body["resourceLogs"].as_array().cloned().unwrap_or_default())
        .filter_map(|r| {
            r["resource"]["attributes"]
                .as_array()?
                .iter()
                .find(|a| a["key"] == "service.name")
                .and_then(|a| a["value"]["stringValue"].as_str().map(str::to_string))
        })
        .collect();
    assert!(
        resource_names.iter().all(|n| n == "dot-agent-deck"),
        "{resource_names:?}"
    );
    // The warning's text, which names the frame length, is in no body.
    for body in receiver.bodies() {
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("exceeds") && !text.contains("connection error"),
            "a log message reached an OTLP body: {text}"
        );
    }
    // The file logger is unchanged: the warning is still in deck.log.
    let log =
        std::fs::read_to_string(deck.workdir().join("deck.log")).expect("captured daemon log");
    assert!(
        log.lines()
            .any(|l| l.contains(" WARN ") && l.contains("attach protocol connection error")),
        "deck.log lost the warning"
    );
}
