#![cfg(all(feature = "e2e", unix))]

//! Synthetic coverage for telemetry handoffs and daemon health.
//! Uses inbox stand-ins and a local OTLP receiver without real agents.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;
#[allow(dead_code)] // Shared receiver operations are not used by this target.
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::Value;
use spec::spec;
use std::time::Duration;
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver, attribute, nanos};

const TASK: &str = "PRIVATE_HANDOFF_TASK_1981_7e093c";
const REPORT: &str = "PRIVATE_HANDOFF_REPORT_1981_58b21a";

fn launch(receiver: &Receiver) -> TuiDeck {
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        // These synthetic tests run CLI components from the test process with
        // fixture-owned capability tokens, rather than inside a real agent.
        .with_env("DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS", "1")
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS", "300000")
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_pty_size(180, 45)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    deck
}

fn cli(deck: &TuiDeck, pane: &inbox::Pane, args: &[&str]) -> String {
    let out = route::cli_output(deck, pane, deck.workdir(), args);
    assert!(
        out.status.success(),
        "CLI {args:?}: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn delegate(deck: &TuiDeck, orch: &inbox::Pane, worker: &inbox::Pane) -> String {
    cli(deck, orch, &["delegate", "--to", "coder", "--task", TASK]);
    let rows = route::count(deck, worker, 1);
    assert_eq!(
        rows[0]["state"], "queued",
        "task must be posted before the worker reads it"
    );
    rows[0]["id"].as_str().unwrap().to_owned()
}

fn handoffs(receiver: &Receiver, id: &str) -> Vec<Value> {
    receiver
        .spans()
        .into_iter()
        .filter(|s| s["name"] == "agent.handoff" && attribute(s, "deck.handoff.id") == Some(id))
        .collect()
}

fn one_handoff(receiver: &Receiver, id: &str) -> Value {
    assert!(
        common::wait_until(EXPORT_WAIT, || !handoffs(receiver, id).is_empty()),
        "no completed agent.handoff for {id}; exported={:?}",
        receiver.spans()
    );
    let mut spans = handoffs(receiver, id);
    assert_eq!(
        spans.len(),
        1,
        "one delegated task must export one handoff: {spans:?}"
    );
    spans.remove(0)
}

fn stop(deck: &TuiDeck, pane: &inbox::Pane) {
    let answer = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: pane.agent.clone(),
        },
    )
    .unwrap();
    assert!(answer.ok, "stop fixture pane: {answer:?}");
}

fn completed_run(receiver: &Receiver, pane: &inbox::Pane) -> Value {
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.run(&pane.id).is_some()),
        "agent.run must be exported for {}",
        pane.id
    );
    receiver.run(&pane.id).unwrap()
}

fn integer(span: &Value, key: &str) -> u64 {
    let value = &span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .unwrap_or_else(|| panic!("missing {key}: {span}"))["value"]["intValue"];
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .unwrap_or_else(|| panic!("{key} must be an OTLP integer: {span}"))
}

fn no_private_payload(receiver: &Receiver) {
    let bodies = receiver.bodies();
    assert!(
        !bodies.is_empty(),
        "privacy assertion needs actual OTLP payloads"
    );
    for body in bodies {
        let body = String::from_utf8(body).expect("HTTP/JSON request body");
        for sentinel in [TASK, REPORT] {
            assert!(
                !body.contains(sentinel),
                "private handoff content in OTLP body: {sentinel}"
            );
        }
    }
}

/// Scenario: Delegate a private task to a stand-in worker, have it fetch and report with the task id, and leave the report unread for an export interval. Only acknowledging the report may complete one handoff, with both lifecycle events, the orchestrator parent, the worker run identity and no private task or report text in the exported payload.
#[spec("telemetry/otlp/038")]
#[test]
fn telemetry_otlp_038_handoff_ends_when_orchestrator_reads_report() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let (orch, worker) = route::team(&deck, false);
    let id = delegate(&deck, &orch, &worker);
    let fetched: Value =
        serde_json::from_str(&cli(&deck, &worker, &["inbox", "fetch", "--json"])).unwrap();
    assert!(
        fetched["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == id && m["text"].as_str().is_some_and(|t| t.contains(TASK)))
    );
    inbox::state(&deck, &worker, &id, "acknowledged");
    cli(
        &deck,
        &worker,
        &["work-done", "--task-id", &id, "--task", REPORT],
    );
    let reports = route::count(&deck, &orch, 1);
    let report_id = reports[0]["id"].as_str().unwrap();
    assert_eq!(reports[0]["provenance"]["task_id"], id);
    assert_eq!(reports[0]["state"], "queued");
    // The existing exporter sends a batch within five seconds. Waiting beyond
    // that makes an implementation that incorrectly ends at t2 observable.
    assert!(
        !common::wait_until(Duration::from_secs(7), || !handoffs(&receiver, &id)
            .is_empty()),
        "handoff exported before the orchestrator read its report"
    );

    // A receipt fetch alone must not stand in for the orchestrator's ack.
    let (_held_receiver, report) = inbox::take(&deck, &orch);
    assert!(
        report["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == report_id)
    );
    inbox::state(&deck, &orch, report_id, "fetched");
    assert!(
        !common::wait_until(Duration::from_secs(7), || !handoffs(&receiver, &id)
            .is_empty()),
        "fetching the report must not finish a handoff before acknowledgement"
    );
    let before_ack = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64;
    cli(&deck, &orch, &["ack", report_id]);
    inbox::state(&deck, &orch, report_id, "acknowledged");
    let handoff = one_handoff(&receiver, &id);
    assert!(
        receiver.run(&orch.id).is_none(),
        "handoff must export while orchestrator is alive"
    );
    assert_eq!(
        attribute(&handoff, "deck.handoff.outcome"),
        Some("reported")
    );
    assert_eq!(attribute(&handoff, "deck.pane.id"), Some(orch.id.as_str()));
    assert_eq!(attribute(&handoff, "deck.role"), Some("coder"));
    assert_eq!(
        attribute(&handoff, "deck.orchestration.id"),
        Some("route-fixture")
    );
    let events = handoff["events"]
        .as_array()
        .expect("handoff lifecycle events");
    let event = |name: &str| {
        let found: Vec<_> = events.iter().filter(|e| e["name"] == name).collect();
        assert_eq!(found.len(), 1, "exactly one {name}: {handoff}");
        found[0]
    };
    let taken = nanos(event("deck.handoff.taken"), "timeUnixNano");
    let reported = nanos(event("deck.handoff.reported"), "timeUnixNano");
    assert!(nanos(&handoff, "startTimeUnixNano") <= taken && taken <= reported);
    assert!(reported <= before_ack && before_ack <= nanos(&handoff, "endTimeUnixNano"));
    stop(&deck, &worker);
    stop(&deck, &orch);
    let worker_run = completed_run(&receiver, &worker);
    let orch_run = completed_run(&receiver, &orch);
    assert_eq!(handoff["traceId"], orch_run["traceId"]);
    assert_eq!(handoff["parentSpanId"], orch_run["spanId"]);
    assert_eq!(
        attribute(&handoff, "deck.run.id"),
        attribute(&orch_run, "deck.run.id")
    );
    assert!(attribute(&worker_run, "deck.run.id").is_some());
    assert_eq!(
        attribute(&handoff, "deck.handoff.worker.run.id"),
        attribute(&worker_run, "deck.run.id")
    );
    assert_eq!(
        handoffs(&receiver, &id),
        vec![handoff],
        "run closure must not export it again"
    );
    no_private_payload(&receiver);
}

/// Scenario: Delegate a private task to a stand-in worker, then terminate that worker before it fetches the task. The completed handoff must identify undeliverable delivery and the agent_exited reason, remain parented to the orchestrator run and exclude the task text from OTLP payloads.
#[spec("telemetry/otlp/039")]
#[test]
fn telemetry_otlp_039_handoff_marks_exited_worker_undeliverable() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let (orch, worker) = route::team(&deck, false);
    let id = delegate(&deck, &orch, &worker);
    // Natural process exit, rather than StopAgent's intentional cancellation.
    let status = std::process::Command::new("kill")
        .args(["-TERM", &worker.pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    // The dead worker may no longer have an attested capability. Inspect the
    // retained message through the live orchestrator's all-inboxes listing.
    assert!(
        common::wait_until(EXPORT_WAIT, || {
            let listing: Value =
                serde_json::from_str(&cli(&deck, &orch, &["inbox", "list", "--all", "--json"]))
                    .unwrap();
            listing["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == id && m["state"] == "undeliverable")
        }),
        "exited worker's posted task must become undeliverable"
    );
    let handoff = one_handoff(&receiver, &id);
    assert_eq!(
        attribute(&handoff, "deck.handoff.outcome"),
        Some("undeliverable")
    );
    assert_eq!(
        attribute(&handoff, "deck.handoff.undeliverable.reason"),
        Some("agent_exited")
    );
    assert_eq!(attribute(&handoff, "deck.pane.id"), Some(orch.id.as_str()));
    assert_eq!(attribute(&handoff, "deck.role"), Some("coder"));
    assert!(
        !handoff["events"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e["name"] == "deck.handoff.reported")
    );
    stop(&deck, &orch);
    let run = completed_run(&receiver, &orch);
    assert_eq!(handoff["traceId"], run["traceId"]);
    assert_eq!(handoff["parentSpanId"], run["spanId"]);
    assert_eq!(handoffs(&receiver, &id), vec![handoff]);
    no_private_payload(&receiver);
}

/// Scenario: Send a note from an uncommissioned stand-in worker through the real work-done CLI without a task id and let the orchestrator fetch it. Closing the orchestrator must export an uncorrelated count of one on its run without inventing a handoff or leaking the note text.
#[spec("telemetry/otlp/040")]
#[test]
fn telemetry_otlp_040_unscoped_work_done_counts_without_inventing_handoff() {
    let receiver = Receiver::start();
    let deck = launch(&receiver);
    let (orch, worker) = route::team(&deck, false);
    cli(&deck, &worker, &["work-done", "--task", REPORT]);
    route::count(&deck, &orch, 1);
    let fetched = cli(&deck, &orch, &["inbox", "fetch", "--json"]);
    assert!(
        fetched.contains(REPORT),
        "control: real report must reach the orchestrator"
    );
    stop(&deck, &worker);
    stop(&deck, &orch);
    let run = completed_run(&receiver, &orch);
    let worker_run = completed_run(&receiver, &worker);
    assert_eq!(integer(&run, "deck.handoffs.uncorrelated.count"), 1);
    assert!(
        receiver
            .spans()
            .iter()
            .all(|s| s["name"] != "agent.handoff"),
        "an uncorrelated report must not manufacture a task span"
    );
    // A worker's completed run is a distinct identity, so the run under test
    // cannot be selected by accidentally taking the first agent.run.
    assert_ne!(
        attribute(&run, "deck.run.id"),
        attribute(&worker_run, "deck.run.id")
    );
    no_private_payload(&receiver);
}
