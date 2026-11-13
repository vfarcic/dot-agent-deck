#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Issue #1835 with a real agent: a Bash command that fails in an interactive
//! Claude Code ends with `PostToolUseFailure`, and the deck must hear it.
//! Issue #2093: a Bash command interrupted with Escape fires no end at all,
//! and the deck must end it when the agent's next turn does.

dot_agent_deck_test_harness::link!();
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
            keep_worktree: false,
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

/// Write `bytes` into the agent's pane through the attach input path, the way
/// a client forwards what a person types.
fn type_into_pane(deck: &TuiDeck, agent_id: &str, bytes: &[u8]) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
            let mut stream = client.attach(agent_id).await.unwrap();
            stream.write_input(bytes).await.unwrap();
            stream.detach().await.unwrap();
        });
}

/// How long typed text must sit in Claude's input box before Enter: an Enter
/// that arrives in the same burst as the text is taken as part of a paste and
/// inserts a newline (the interval `tests/e2e_prompt_keys_live.rs` uses).
const BEFORE_SUBMIT: Duration = Duration::from_millis(750);

fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// Scenario: Start trusted interactive Claude Haiku in a real attached pane and ask it to run, in the foreground, a script that writes a uniquely named start marker, sleeps 20 seconds and then writes a finish marker. Once the start marker appears, press Escape in the pane as a person interrupting it would and wait for Claude to show the call Interrupted, then type a request for a one-word reply into the pane so its next turn ends. The script's operation must export while the run is open, marked `interrupted` and ending at the Escape; the finish marker must never appear.
#[spec("telemetry/otlp/050")]
#[test]
fn telemetry_otlp_050_real_claude_interrupted_command_ends_at_the_escape() {
    skip_unless!(common::check_claude_available());
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "interrupted-command-haiku",
            "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write",
        )
        .launch_with_fixture("chain-smoke-claude");
    const SCRIPT: &str = "interrupt_probe_2093.sh";
    const STARTED: &str = "interrupt_probe_2093_started.txt";
    const FINISHED: &str = "interrupt_probe_2093_finished.txt";
    let script = deck.workdir().join(SCRIPT);
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho STARTED > {STARTED}\nsleep 20\necho FINISHED > {FINISHED}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    deck.wait_for_string("Claude Code v");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| agent.display_name.as_deref() == Some("interrupted-command-haiku"))
        .expect("real interactive Haiku pane");
    let pane = agent.pane_id_env.as_deref().expect("pane id").to_string();
    let directive = format!(
        "Use the Bash tool exactly once to run this exact command: ./{SCRIPT} . It takes about 20 seconds. Run it in the foreground: do not set run_in_background, and wait for it. Do not run anything else."
    );
    common::post_message_on(deck.attach_socket_path(), &pane, &agent.id, &directive)
        .expect("post the slow task to Claude");
    common::wait_for_file_trimmed_eq(
        &deck.workdir().join(STARTED),
        "STARTED",
        Duration::from_secs(120),
    )
    .expect("real Claude must start the script, which writes the unique start marker");

    // Escape through the attach input path, the way a client forwards a key a
    // person presses in the pane.
    let before_escape = unix_nanos_now();
    type_into_pane(&deck, &agent.id, b"\x1b");
    let after_escape = unix_nanos_now();
    // Claude Code's own word that the Escape stopped the call: the positive
    // proof the command did not run to its end.
    assert!(
        common::wait_until(Duration::from_secs(30), || deck
            .snapshot_grid()
            .contains("Interrupted")),
        "Claude must show the call interrupted:\n{}",
        deck.snapshot_grid()
    );

    // Typed, as the person who interrupted would: the deck holds its own
    // messages for a card still Working, and an interrupted turn leaves the
    // card Working, since Claude Code fires no `Stop` for it.
    // Enter goes in once the text has sat in Claude's input box for
    // `BEFORE_SUBMIT`, so Claude reads it as a key after the typing rather
    // than as a newline inside one paste.
    type_into_pane(
        &deck,
        &agent.id,
        b"Do not run any command. Reply INTERRUPT_2093_DONE and end your turn.",
    );
    deck.wait_until_grid_then_hold(
        "the typed follow-up settled in Claude's input box",
        BEFORE_SUBMIT,
        |grid| grid.contains("Reply INTERRUPT_2093_DONE"),
    );
    type_into_pane(&deck, &agent.id, b"\r");
    // The probe script's own operation, however Haiku spelled the command
    // (`./interrupt_probe_2093.sh`, `sh interrupt_probe_2093.sh`, …).
    let is_probe = |op: &serde_json::Value| {
        [
            "deck.operation.program",
            "deck.operation.word",
            "deck.operation.commands",
        ]
        .iter()
        .any(|key| attribute(op, key).is_some_and(|v| v.contains("interrupt_probe_2093")))
    };
    let probe = || {
        receiver
            .operations(&pane)
            .into_iter()
            .find(|op| is_probe(op))
    };
    assert!(
        common::wait_until(Duration::from_secs(120), || probe().is_some()),
        "the interrupted script's operation must export once Claude's next turn ends (issue \
         #2093); exported: {:?}\n{}",
        receiver.operations(&pane),
        deck.snapshot_grid()
    );
    assert!(
        receiver.run(&pane).is_none(),
        "the operation must finish while the run is open"
    );
    let interrupted = probe().unwrap();
    assert_eq!(
        attribute(&interrupted, "deck.operation.end"),
        Some("interrupted"),
        "{interrupted}"
    );
    let end = nanos(&interrupted, "endTimeUnixNano");
    assert!(
        (before_escape..=after_escape + 5_000_000_000).contains(&end),
        "the span must end at the Escape, not at the next turn's end: {interrupted}"
    );
    assert!(
        receiver
            .operations(&pane)
            .iter()
            .all(|op| attribute(op, "deck.operation.end") != Some("run_end")),
        "no operation may wait for the run's end: {:?}",
        receiver.operations(&pane)
    );
    // Claude showed the call `Interrupted` above; the script's own finish
    // marker agrees.
    assert!(
        !deck.workdir().join(FINISHED).exists(),
        "the Escape must have stopped the script"
    );

    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
            keep_worktree: false,
        },
    )
    .unwrap();
    assert!(response.ok, "close Claude pane: {response:?}");
}
