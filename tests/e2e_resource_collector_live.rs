#![cfg(all(feature = "e2e", feature = "e2e-live", target_os = "linux"))]

//! Real interactive work joins the collector's figures to the deck run trace.

mod common;
#[allow(dead_code)]
mod resource_collector;

use common::TuiDeck;
use resource_collector::{Receiver, assert_figures, attribute, close, cpu, start_collector};
use spec::spec;
use std::time::Duration;

/// Scenario: Start trusted interactive Claude Haiku in an attached pane and collect a baseline before asking it to copy a uniquely named fixture through Bash. Observe Thinking, Working/Bash and Idle plus the sentinel proof, then require a later collector sample with CPU growth, positive anonymous PSS and named anon/file/shmem points sharing the deck's actual agent.run identity.
#[spec("telemetry/resources/004")]
#[test]
fn resources_004_real_interactive_haiku_figures_join_deck_run() {
    skip_unless!(common::check_claude_available());
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        // This scenario joins the collector and deck, so native Claude exports
        // need not add a third producer to this receiver.
        .with_env("CLAUDE_CODE_ENABLE_TELEMETRY", "0")
        .with_continue_session(
            "resource-haiku",
            "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write",
        )
        .launch_with_fixture("chain-smoke-claude");
    const SENTINEL: &str = "resource_collector_sentinel_c92d7a.txt";
    const PROOF: &str = "resource_collector_proof_c92d7a.txt";
    std::fs::write(deck.workdir().join(SENTINEL), "RESOURCE_REAL_AGENT_OK\n").unwrap();
    deck.wait_for_string("Claude Code v");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| agent.display_name.as_deref() == Some("resource-haiku"))
        .expect("real interactive Haiku pane");
    let pane = agent.pane_id_env.as_deref().unwrap();
    // Independent deadline bounds the collector even if nextest kills this
    // test process. No real-agent credential is needed by the collector.
    let mut collector = start_collector(&receiver);
    let pane_samples = || {
        receiver
            .metrics()
            .into_iter()
            .filter(|entry| {
                attribute(&entry["resource"], "deck.pane.id") == pane
                    && !attribute(&entry["resource"], "deck.run.id").is_empty()
            })
            .collect::<Vec<_>>()
    };
    assert!(
        common::wait_until(Duration::from_secs(25), || !pane_samples().is_empty()),
        "collector must export a baseline before the agent task"
    );
    let baseline = pane_samples().last().unwrap().clone();
    let baseline_cpu = cpu(&baseline);
    let start = deck.stream_text().len();
    let directive = format!(
        "Use Bash exactly once to run: sleep 3; cat '{}/{SENTINEL}' > '{}/{PROOF}'. Do not modify any other file. Then reply RESOURCE_DONE and end your turn.",
        deck.workdir().display(),
        deck.workdir().display(),
    );
    common::post_message_on(deck.attach_socket_path(), pane, &agent.id, &directive)
        .expect("post sentinel task to real Claude");
    assert!(
        common::wait_until(Duration::from_secs(120), || {
            let stream = deck.stream_text();
            let turn = stream.get(start..).unwrap_or_default();
            common::match_needles_in_order(
                turn.as_bytes(),
                &["Thinking", "Working", "Bash", "Idle"],
            ) == 4
        }),
        "real Claude must visibly complete Thinking → Working/Bash → Idle:\n{}",
        deck.snapshot_grid()
    );
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        "RESOURCE_REAL_AGENT_OK",
        Duration::from_secs(10),
    )
    .expect("real Claude must copy the unique fixture sentinel");
    assert!(
        collector.0.try_wait().unwrap().is_none(),
        "collector must keep sampling during real work"
    );
    assert!(
        common::wait_until(Duration::from_secs(25), || {
            let samples = pane_samples();
            samples.len() >= 2
                && samples
                    .last()
                    .is_some_and(|entry| cpu(entry) > baseline_cpu)
        }),
        "collector must export at least two samples with run CPU growth during the real Haiku task (baseline {baseline_cpu})"
    );
    let metrics = receiver.metrics();
    let resource = metrics
        .iter()
        .rev()
        .find(|entry| attribute(&entry["resource"], "deck.pane.id") == pane)
        .unwrap();
    assert_figures(resource, 0.0);
    assert!(
        cpu(resource) > baseline_cpu,
        "task CPU must exceed the pre-task baseline"
    );
    assert_eq!(
        attribute(&resource["resource"], "deck.run.id"),
        attribute(&baseline["resource"], "deck.run.id")
    );
    close(&deck, &agent);
    let span = receiver.run(pane);
    assert_eq!(
        attribute(&span, "deck.run.id"),
        attribute(&resource["resource"], "deck.run.id")
    );
}
