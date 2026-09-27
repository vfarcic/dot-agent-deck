#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Real-agent checks for non-quota API failures in the Blocked status path.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::{AgentType, EventType};
use spec::spec;

fn attached_control(viewer: &TuiDeck) -> TuiDeck {
    TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            viewer.attach_socket_path().to_string_lossy(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            viewer.hook_socket_path().to_string_lossy(),
        )
        .without_success_recording()
        .launch_with_fixture("minimal")
}

fn start_agent(control: &TuiDeck, viewer: &TuiDeck, name: &str, command: &str) -> String {
    control.send_keys(b"\x0e");
    control.wait_for_string("Select Directory");
    control.send_keys(b" ");
    control.wait_for_string("New Agent");
    control.send_keys(b"\t");
    control.send_keys(name.as_bytes());
    control.send_keys(b"\t");
    control.send_keys(command.as_bytes());
    let (col, row) = control.wait_for_in_grid("[Submit]");
    control.click(col, row);

    assert!(
        common::wait_until(Duration::from_secs(15), || {
            common::agent_records_on(viewer.attach_socket_path())
                .iter()
                .any(|record| {
                    record
                        .display_name
                        .as_deref()
                        .is_some_and(|display| display.ends_with(name))
                })
        }),
        "agent {name} was not registered: {:?}; control grid:\n{}",
        common::agent_records_on(viewer.attach_socket_path()),
        control.snapshot_grid()
    );
    common::agent_records_on(viewer.attach_socket_path())
        .into_iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|display| display.ends_with(name))
        })
        .expect("agent found in wait")
        .id
}

fn assert_error_card(viewer: &TuiDeck, name: &str) {
    assert!(
        viewer.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| {
            grid.lines()
                .any(|line| line.contains(name) && line.contains("Error"))
        }),
        "{name} did not show an Error card:\n{}",
        viewer.snapshot_grid()
    );
    assert!(
        !viewer.snapshot_grid().contains("Blocked"),
        "a non-quota error must never show Blocked:\n{}",
        viewer.snapshot_grid()
    );
}

/// Scenario: Start a genuine interactive Claude session with a retired model,
/// submit one prompt, and observe its StopFailure as an Error event and Error
/// card. The provider failure must not leave the card Thinking or Blocked.
#[spec("status/blocked/022")]
#[test]
fn status_blocked_022_real_claude_api_failure_ends_error_not_thinking() {
    skip_unless!(common::check_claude_available());

    let viewer = TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    viewer.wait_for_string("No active sessions");
    let control = attached_control(&viewer);
    control.wait_for_string("No active sessions");
    let cwd = control.workdir().to_path_buf();
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(viewer.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and project trust");

    let events = viewer.subscribe_events();
    let name = "claude-err";
    let agent_id = start_agent(
        &control,
        &viewer,
        name,
        "claude --model claude-3-7-sonnet-20250219 --allowedTools Bash",
    );
    assert!(
        control.wait_for_grid_string_within("Claude Code v", Duration::from_secs(45)),
        "real Claude did not reach its prompt:\n{}",
        control.snapshot_grid()
    );
    control.send_keys(b"Reply with the word hello.\r");
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.agent_type == AgentType::ClaudeCode
                && event.event_type == EventType::Error
        },
        Duration::from_secs(90),
    );
    assert_error_card(&viewer, name);
}

/// Scenario: Start a genuine interactive OpenCode session with an unsupported
/// model, submit one prompt, and observe the real plugin's session.error as an
/// Error event and Error card. The non-quota failure must not show Blocked.
#[spec("status/blocked/023")]
#[test]
fn status_blocked_023_real_opencode_api_error_is_forwarded_not_blocked() {
    skip_unless!(common::check_opencode_available());

    let viewer = TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_imported_opencode_credentials()
        .launch_with_fixture("minimal");
    viewer.wait_for_string("No active sessions");
    let control = attached_control(&viewer);
    control.wait_for_string("No active sessions");

    let events = viewer.subscribe_events();
    let name = "opencode-err";
    let agent_id = start_agent(
        &control,
        &viewer,
        name,
        "opencode --model openai/gpt-5.4-mini",
    );
    assert!(
        control.wait_for_grid_string_within("Ask anything", Duration::from_secs(45)),
        "real OpenCode did not reach its prompt:\n{}",
        control.snapshot_grid()
    );
    control.send_keys(b"Reply with the word hello.\r");
    events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(agent_id.as_str())
                && event.agent_type == AgentType::OpenCode
                && event.event_type == EventType::Error
        },
        Duration::from_secs(90),
    );
    assert_error_card(&viewer, name);
}
