#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! Real interactive Haiku discovers and messages another orchestration's leader.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::EventType;
use dot_agent_deck::inbox::{BodyTrust, Sender};
use spec::spec;

fn wait_for_agent(deck: &TuiDeck, orchestrator: bool) -> AgentRecord {
    let matches = |record: &AgentRecord| {
        if orchestrator {
            matches!(
                record.tab_membership,
                Some(TabMembership::Orchestration {
                    is_start_role: true,
                    ..
                })
            )
        } else {
            record.tab_membership.is_none()
                && record.display_name.as_deref() == Some("outside-haiku")
        }
    };
    assert!(
        common::wait_until(Duration::from_secs(90), || {
            common::agent_records_on(deck.attach_socket_path())
                .iter()
                .any(matches)
        }),
        "interactive agent did not spawn\n{}",
        deck.snapshot_grid()
    );
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(matches)
        .expect("agent")
}

/// Scenario: Open real interactive Haiku agents and wait for the orchestrator's startup turn to finish before choosing a marker it does not know. The standalone caller runs roles and inbox send, acknowledges sending, and the recipient's inbox attributes the marker to that caller; the orchestrator copies it to a sentinel file and visibly acknowledges receipt.
#[spec("orchestration/address/005")]
#[test]
fn orchestration_address_005_real_haiku_discovers_and_messages_orchestrator() {
    skip_unless!(common::check_claude_available());
    let deck = TuiDeck::builder()
        .with_pty_size(200, 55)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("PATH", common::path_with_binary_dir())
        .launch_with_fixture("orchestrator-address");
    deck.wait_for_string("No active agents");
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_absence("[Submit]");
    let orchestrator = wait_for_agent(&deck, true);
    let recipient_session = events.wait_for_session_start_on_pane(
        orchestrator.pane_id_env.as_deref().unwrap(),
        &orchestrator.id,
        Duration::from_secs(90),
    );
    assert!(
        common::wait_for_pane_text_on(
            deck.attach_socket_path(),
            &orchestrator.id,
            "ADDRESS-READY-1947-7b91",
            Duration::from_secs(90),
        ),
        "recipient did not announce startup readiness"
    );
    // Claude's native Stop hook emits Idle. Unlike SessionStart or a default
    // ListAgents Idle value, this proves the startup response has finished.
    events.wait_for(
        |event| {
            event.event_type == EventType::Idle
                && !event.is_from_subagent()
                && event.agent_id.as_deref() == Some(orchestrator.id.as_str())
                && event.pane_id == orchestrator.pane_id_env
                && event.session_id == recipient_session
        },
        Duration::from_secs(90),
    );

    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\r");
    deck.send_keys(&[0x7f; 96]);
    deck.send_keys(b"outside-haiku");
    deck.send_keys(b"\r");
    deck.send_keys(&[0x7f; 512]);
    deck.send_keys(b"claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write");
    deck.send_keys(b"\r");
    deck.wait_for_absence("[Submit]");
    let caller = wait_for_agent(&deck, false);
    events.wait_for_session_start_on_pane(
        caller.pane_id_env.as_deref().unwrap(),
        &caller.id,
        Duration::from_secs(90),
    );
    let sentinel = deck.workdir().join("address_received_1947_7b91.txt");
    assert!(
        !sentinel.exists(),
        "recipient acted before receiving outside data"
    );
    let failure = deck.workdir().join("address_cli_failure_1947_7b91.txt");
    let marker = format!(
        "ADDRESS_LIVE_1947_{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let folder = deck.workdir().display().to_string();
    let directive = format!(
        "This is a controlled messaging test. Use Bash to run dot-agent-deck roles --json. \
         From the output find the orchestrator (orchestrator=true) of folder {folder}. \
         Then run dot-agent-deck inbox send --folder '{folder}' --text '{marker}'. \
         You are a standalone caller, outside that orchestration. Do not create the recipient's file yourself, \
         delegate, or run work-done. On success visibly print ADDRESS-SENT- followed immediately by 1947-7b91 with no space. \
         If either command fails, write the exact stderr to '{}' and stop; do not work around it or ask questions.",
        failure.display()
    );
    common::post_message_on(
        deck.attach_socket_path(),
        caller.pane_id_env.as_deref().unwrap(),
        &caller.id,
        &directive,
    )
    .expect("direct the real outside caller");
    assert!(
        common::wait_until(Duration::from_secs(180), || sentinel.exists()
            || failure.exists()),
        "real caller did not complete its discovery/send attempt\nCaller:\n{}\nOrchestrator:\n{}",
        common::pane_search_key_on(deck.attach_socket_path(), &caller.id),
        common::pane_search_key_on(deck.attach_socket_path(), &orchestrator.id)
    );
    assert!(
        !failure.exists(),
        "real caller's roles/inbox send failed: {}",
        std::fs::read_to_string(&failure).unwrap_or_default()
    );
    common::wait_for_file_trimmed_eq(&sentinel, &marker, Duration::from_secs(20))
        .expect("real orchestrator must record the received marker");
    let inbox = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::ListInbox {
            pane_id: orchestrator.pane_id_env.clone(),
            bodies: true,
        },
    )
    .expect("read recipient inbox")
    .inbox_messages
    .expect("inbox messages");
    assert!(
        inbox.iter().any(|message| {
            message.body_trust == BodyTrust::Untrusted
                && message
                    .text
                    .as_deref()
                    .is_some_and(|body| body.contains(&marker))
                && matches!(&message.from, Sender::Agent { pane_id, agent_id, .. }
                if Some(pane_id) == caller.pane_id_env.as_ref() && agent_id == &caller.id)
        }),
        "recipient inbox lacks the runtime marker from the expected outside caller"
    );
    // Native Bash tool hooks, scoped to this caller's pane and generation,
    // establish command execution rather than matching the directive's echo.
    for command in ["dot-agent-deck roles --json", "dot-agent-deck inbox send"] {
        let executed = events.wait_for(
            |event| {
                event.event_type == EventType::ToolStart
                    && event.agent_id.as_deref() == Some(caller.id.as_str())
                    && event.pane_id == caller.pane_id_env
                    && event
                        .metadata
                        .get("bash_command")
                        .is_some_and(|text| text.contains(command))
            },
            Duration::from_secs(30),
        );
        eprintln!(
            "outside caller pane {} agent {} executed Bash: {}",
            caller.pane_id_env.as_deref().unwrap(),
            caller.id,
            executed.metadata["bash_command"]
        );
    }
    assert!(
        common::wait_for_pane_text_on(
            deck.attach_socket_path(),
            &caller.id,
            "ADDRESS-SENT-1947-7b91",
            Duration::from_secs(60),
        ),
        "caller did not acknowledge sending\n{}",
        common::pane_search_key_on(deck.attach_socket_path(), &caller.id)
    );
    deck.send_keys(b"\x1b[5;5~");
    assert!(
        common::wait_for_pane_text_on(
            deck.attach_socket_path(),
            &orchestrator.id,
            "ADDRESS-RECEIVED-1947-7b91",
            Duration::from_secs(60)
        ),
        "orchestrator did not visibly acknowledge receipt\n{}",
        common::pane_search_key_on(deck.attach_socket_path(), &orchestrator.id)
    );
}
