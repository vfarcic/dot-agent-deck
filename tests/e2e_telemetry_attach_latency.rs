#![cfg(all(feature = "e2e", unix))]

//! Synthetic coverage for the attach latency on `daemon.health` (#1981 M8).
//! No real agent is started; attaches go through the real attach socket and
//! health through the real export pipeline.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;
#[allow(dead_code)] // This target uses the receiver's general span view.
mod telemetry_operation_receiver;

use common::TuiDeck;
use serde_json::Value;
use spec::spec;
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver, nanos};

const ATTACHES: u64 = 3;

fn launch(receiver: &Receiver) -> TuiDeck {
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_env("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1")
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        // Shorten the health sampling period for the synthetic fixture.
        .with_env("DOT_AGENT_DECK_TELEMETRY_HEALTH_INTERVAL_MS", "250")
        .with_pty_size(180, 45)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    deck
}

fn health(receiver: &Receiver) -> Vec<Value> {
    receiver
        .spans()
        .into_iter()
        .filter(|s| s["name"] == "daemon.health")
        .collect()
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .map(|a| &a["value"])
}

fn integer(span: &Value, key: &str) -> u64 {
    let value = &attr(span, key).unwrap_or_else(|| panic!("missing {key}: {span}"))["intValue"];
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .unwrap_or_else(|| panic!("{key} must be a nonnegative OTLP integer: {span}"))
}

/// The attaches counted by the health spans that ended after `since`.
fn attaches_since(receiver: &Receiver, since: u64) -> u64 {
    health(receiver)
        .iter()
        .filter(|s| nanos(s, "endTimeUnixNano") > since)
        .map(|s| integer(s, "deck.daemon.attach_latency.count"))
        .sum()
}

/// Scenario: Start a deck with a short health interval and two stand-in roles, then attach to the orchestrator's terminal three times over the real attach socket, as a client showing it does. The daemon.health spans after the attaches must count at least those three with ordered median, 95th percentile and maximum, and an interval with no attach must leave the quantiles out.
#[spec("telemetry/otlp/043")]
#[test]
fn telemetry_otlp_043_attaches_are_timed_on_daemon_health() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let (orch, _worker) = route::team(&deck, false);
    let before = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64;
    let attached: Vec<_> = (0..ATTACHES)
        .map(|_| common::AttachInput::attach(deck.attach_socket_path(), &orch.agent))
        .collect();
    assert!(
        common::wait_until(EXPORT_WAIT, || attaches_since(&receiver, before)
            >= ATTACHES),
        "the attaches were never counted; health={:?}",
        health(&receiver)
    );
    drop(attached);
    for span in health(&receiver) {
        if integer(&span, "deck.daemon.attach_latency.count") == 0 {
            for key in [
                "deck.daemon.attach_latency.p50_ms",
                "deck.daemon.attach_latency.p95_ms",
                "deck.daemon.attach_latency.max_ms",
            ] {
                assert!(
                    attr(&span, key).is_none(),
                    "an interval with no attach must omit {key}: {span}"
                );
            }
        } else {
            let p50 = integer(&span, "deck.daemon.attach_latency.p50_ms");
            let p95 = integer(&span, "deck.daemon.attach_latency.p95_ms");
            let max = integer(&span, "deck.daemon.attach_latency.max_ms");
            assert!(p50 <= p95 && p95 <= max, "unordered quantiles: {span}");
        }
    }
    assert!(
        health(&receiver)
            .iter()
            .any(|s| integer(s, "deck.daemon.attach_latency.count") == 0),
        "some interval must have had no attach"
    );
}
