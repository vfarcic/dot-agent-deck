#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Issue #1835 with a real agent: a Bash command that fails in an interactive
//! Claude Code ends with `PostToolUseFailure`, and the deck must hear it.

mod common;
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;
use std::time::Duration;
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver, attribute, nanos};

/// Scenario: Start trusted interactive Claude Haiku in a real attached pane and ask it to run one Bash command that copies a uniquely named sentinel with `cp` and then exits 3, visibly completing its turn. That command's exported `cp` operation must end while the run is still open and without the deck's forced-closure attribute, and the card must finish Idle rather than Error.
#[spec("telemetry/otlp/046")]
#[test]
fn telemetry_otlp_046_real_claude_failed_command_ends_its_operation() {
    skip_unless!(common::check_claude_available());
    // The deck installs the hook under test only for a Claude Code at or above
    // its floor; below it, a failed call still ends at run end, by design.
    let (gates, version) = dot_agent_deck::hooks_manage::installed_claude_hook_gates();
    skip_unless!(if gates.post_tool_use_failure {
        Ok(())
    } else {
        Err(format!(
            "the deck installs PostToolUseFailure only for Claude Code 2.1.78 or newer; \
             `claude --version` reported {version:?}"
        ))
    });
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "failed-command-haiku",
            "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write",
        )
        .launch_with_fixture("chain-smoke-claude");
    const SENTINEL: &str = "failed_op_1835_sentinel_7c2d.txt";
    const PROOF: &str = "failed_op_1835_proof_7c2d.txt";
    std::fs::write(deck.workdir().join(SENTINEL), "FAILED_OPERATION_RAN\n").unwrap();
    deck.wait_for_string("Claude Code v");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| agent.display_name.as_deref() == Some("failed-command-haiku"))
        .expect("real interactive Haiku pane");
    let pane = agent.pane_id_env.as_deref().expect("pane id");
    let start = deck.stream_text().len();
    // Plain words only, so the deck records `cp` as the operation's program
    // and the test can tell this command's span from any other.
    let directive = format!(
        "In the current directory, use Bash exactly once to run this exact command: cp {SENTINEL} {PROOF}; exit 3. The command is meant to fail with exit code 3; do not retry it or run anything else. Then reply FAILED_OP_DONE and end your turn."
    );
    common::post_message_on(deck.attach_socket_path(), pane, &agent.id, &directive)
        .expect("post failing task to Claude");
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(PROOF),
        "FAILED_OPERATION_RAN",
        Duration::from_secs(120),
    )
    .expect("real Claude must run the failing command, which copies the unique sentinel");
    assert!(
        common::wait_until(Duration::from_secs(120), || {
            let stream = deck.stream_text();
            let turn = stream.get(start..).unwrap_or_default();
            common::match_needles_in_order(turn.as_bytes(), &["Working", "Bash", "Idle"]) == 3
        }),
        "real Claude must visibly complete Working/Bash → Idle:\n{}",
        deck.snapshot_grid()
    );
    let failed = || {
        receiver
            .operations(pane)
            .into_iter()
            .find(|operation| attribute(operation, "deck.operation.program") == Some("cp"))
    };
    assert!(
        common::wait_until(EXPORT_WAIT, || failed().is_some()),
        "the failed `cp …; exit 3` operation must export when Claude ends the call, not at \
         run end (issue #1835); exported: {:?}",
        receiver.operations(pane)
    );
    assert!(
        receiver.run(pane).is_none(),
        "the operation must finish while the run is open"
    );
    let failed = failed().unwrap();
    assert!(
        attribute(&failed, "deck.operation.end").is_none(),
        "Claude ended this call itself, so the deck must not have closed it: {failed}"
    );
    assert!(
        !deck.snapshot_grid().contains("Error"),
        "a failed command is ordinary agent work, not an Error card:\n{}",
        deck.snapshot_grid()
    );
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(response.ok, "close Claude pane: {response:?}");
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.run(pane).is_some()),
        "run never exported"
    );
    assert!(
        nanos(&failed, "endTimeUnixNano") < nanos(&receiver.run(pane).unwrap(), "endTimeUnixNano"),
        "the failed command must have ended before the run did: {failed}"
    );
}
