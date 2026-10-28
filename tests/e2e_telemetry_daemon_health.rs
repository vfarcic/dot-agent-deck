#![cfg(all(feature = "e2e", unix))]

//! Synthetic coverage for telemetry handoffs and daemon health.
//! No real agent is started; health is observed through the real export pipeline.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;
#[allow(dead_code)] // This target uses the receiver's general span view.
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::EventType;
use serde_json::{Value, json};
use spec::spec;
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver, nanos};

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

fn number(span: &Value, key: &str) -> f64 {
    let value = attr(span, key).unwrap_or_else(|| panic!("missing {key}: {span}"));
    let number = value["doubleValue"]
        .as_f64()
        .or_else(|| value["intValue"].as_f64())
        .or_else(|| value["intValue"].as_str()?.parse().ok())
        .unwrap_or_else(|| panic!("{key} must be an OTLP number: {span}"));
    assert!(number.is_finite() && number >= 0.0, "invalid {key}: {span}");
    number
}

fn wait_health(receiver: &Receiver, pred: impl Fn(&Value) -> bool) -> Value {
    assert!(
        common::wait_until(EXPORT_WAIT, || health(receiver).iter().any(&pred)),
        "no matching daemon.health span; exported={:?}",
        receiver.spans()
    );
    health(receiver).into_iter().find(pred).unwrap()
}

fn validate(span: &Value) {
    assert!(nanos(span, "startTimeUnixNano") <= nanos(span, "endTimeUnixNano"));
    integer(span, "deck.daemon.runs.open");
    let count = integer(span, "deck.daemon.hook_latency.count");
    integer(span, "deck.telemetry.spans.dropped");
    if count == 0 {
        for key in [
            "deck.daemon.hook_latency.p50_ms",
            "deck.daemon.hook_latency.p95_ms",
            "deck.daemon.hook_latency.max_ms",
        ] {
            assert!(
                attr(span, key).is_none(),
                "empty interval must omit {key}: {span}"
            );
        }
    } else {
        let p50 = number(span, "deck.daemon.hook_latency.p50_ms");
        let p95 = number(span, "deck.daemon.hook_latency.p95_ms");
        let max = number(span, "deck.daemon.hook_latency.max_ms");
        assert!(
            p50 <= p95 && p95 <= max,
            "invalid percentile ordering: {span}"
        );
    }
    #[cfg(target_os = "linux")]
    assert!(
        integer(span, "deck.daemon.memory.rss") > 0,
        "Linux RSS in bytes must be positive"
    );
    // RSS availability on other operating systems is intentionally unspecified.
}

fn stop(deck: &TuiDeck, pane: &inbox::Pane) {
    let reply = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: pane.agent.clone(),
        },
    )
    .unwrap();
    assert!(reply.ok, "stop stand-in: {reply:?}");
}

/// Scenario: Start an attached deck with a short health interval and two stand-in roles, then stop one role. Periodic OTLP health spans must expose memory on Linux, open-run counts that change from two to one, interval latency summaries and a zero drop count under this small load.
#[spec("telemetry/otlp/041")]
#[test]
fn telemetry_otlp_041_periodic_health_exports_daemon_measurements() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let (orch, worker) = route::team(&deck, false);
    let two = wait_health(&receiver, |s| integer(s, "deck.daemon.runs.open") == 2);
    validate(&two);
    assert_eq!(integer(&two, "deck.telemetry.spans.dropped"), 0);
    let two_end = nanos(&two, "endTimeUnixNano");
    stop(&deck, &worker);
    let stopped_at = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64;
    let one = wait_health(&receiver, |s| {
        nanos(s, "endTimeUnixNano") > stopped_at && integer(s, "deck.daemon.runs.open") == 1
    });
    validate(&one);
    assert!(
        nanos(&one, "endTimeUnixNano") > two_end,
        "health must be periodic"
    );
    assert_eq!(integer(&one, "deck.telemetry.spans.dropped"), 0);
    stop(&deck, &orch);
}

/// Scenario: Observe an empty health interval, start a stand-in pane, deliver one producer Thinking hook timestamped in the past and stop the pane, observing the daemon-authored close event. Health intervals covering those actions must count the producer once, exclude the daemon events, retain its measured delay and reset the latency summary in a later quiet interval.
#[spec("telemetry/otlp/042")]
#[test]
fn telemetry_otlp_042_hook_latency_counts_producers_and_resets_each_interval() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let baseline = wait_health(&receiver, |_| true);
    validate(&baseline);
    assert_eq!(integer(&baseline, "deck.daemon.hook_latency.count"), 0);
    let baseline_end = nanos(&baseline, "endTimeUnixNano");
    let events = deck.subscribe_events();
    let pane = route::start(&deck, "health-producer", json!({}));
    let sent_at = chrono::Utc::now();
    let producer = json!({
        "session_id": format!("health-producer-{}", pane.agent),
        "pane_id": pane.id, "agent_id": pane.agent, "token": pane.token,
        "agent_type": "claude_code", "event_type": "thinking",
        "timestamp": sent_at - chrono::Duration::milliseconds(1500),
        "cwd": deck.workdir()
    });
    common::write_hook_line(deck.hook_socket_path(), &producer.to_string()).unwrap();
    let observed = events.wait_for(
        |e| {
            e.event_type == EventType::Thinking
                && e.agent_id.as_deref() == Some(pane.agent.as_str())
        },
        EXPORT_WAIT,
    );
    assert!(
        !observed.is_daemon_synthetic(),
        "the positive sample must be a producer"
    );
    stop(&deck, &pane);
    let synthetic = events.wait_for(
        |e| {
            e.event_type == EventType::SessionEnd
                && e.pane_id.as_deref() == Some(pane.id.as_str())
                && e.is_daemon_synthetic()
        },
        EXPORT_WAIT,
    );
    // Phase-independent: actions may straddle health ticks. Sum a completed
    // interval range rather than expecting both events to fit the same tick.
    let after_synthetic = synthetic.timestamp.timestamp_nanos_opt().unwrap() as u64;
    let boundary = wait_health(&receiver, |s| {
        nanos(s, "endTimeUnixNano") > after_synthetic && integer(s, "deck.daemon.runs.open") == 0
    });
    let boundary_end = nanos(&boundary, "endTimeUnixNano");
    let samples: Vec<_> = health(&receiver)
        .into_iter()
        .filter(|s| {
            let end = nanos(s, "endTimeUnixNano");
            end > baseline_end && end <= boundary_end
        })
        .collect();
    let count: u64 = samples
        .iter()
        .map(|s| integer(s, "deck.daemon.hook_latency.count"))
        .sum();
    assert_eq!(
        count, 1,
        "one producer hook and daemon start/close events must yield one sample: {samples:?}"
    );
    let measured = samples
        .iter()
        .find(|s| integer(s, "deck.daemon.hook_latency.count") == 1)
        .unwrap();
    validate(measured);
    for key in [
        "deck.daemon.hook_latency.p50_ms",
        "deck.daemon.hook_latency.p95_ms",
        "deck.daemon.hook_latency.max_ms",
    ] {
        assert!(
            number(measured, key) >= 1400.0,
            "timestamped producer delay was lost or diluted by synthetic events: {measured}"
        );
    }
    let quiet = wait_health(&receiver, |s| {
        nanos(s, "startTimeUnixNano") >= boundary_end && nanos(s, "endTimeUnixNano") > boundary_end
    });
    validate(&quiet);
    assert_eq!(
        integer(&quiet, "deck.daemon.hook_latency.count"),
        0,
        "latency count is per interval"
    );
    for key in [
        "deck.daemon.hook_latency.p50_ms",
        "deck.daemon.hook_latency.p95_ms",
        "deck.daemon.hook_latency.max_ms",
    ] {
        assert!(
            attr(&quiet, key).is_none(),
            "quiet interval must omit {key}"
        );
    }
}
