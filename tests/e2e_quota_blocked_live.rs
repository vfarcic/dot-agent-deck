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
    // The dashboard button bar also says "New Agent". Wait for the form's
    // bordered title so the keystrokes below cannot land before it is up.
    control.wait_for_string("┌ New Agent");
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
    viewer.wait_for_string("No active agents");
    let control = attached_control(&viewer);
    control.wait_for_string("No active agents");
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
    viewer.wait_for_string("No active agents");
    let control = attached_control(&viewer);
    control.wait_for_string("No active agents");

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

/// Codex's empty-composer placeholder — see `CODEX_COMPOSER_READY` in
/// `tests/e2e_codex_hooks.rs` for why it is a sound readiness gate.
const CODEX_COMPOSER_READY: &str = "Ask Codex to do anything";

/// A model no Codex account can use, so the provider rejects the turn with a
/// non-quota error before any token is spent (issue #1359).
const CODEX_REJECTED_MODEL: &str = "gpt-nonexistent-model-1359";

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = std::path::Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Scenario: Start a genuine interactive Codex session with a model the
/// provider rejects, submit one prompt, and observe the failed turn — which
/// Codex reports through no hook, only its rollout's `task_complete` — as an
/// Error event and Error card. It must not stay Thinking or show Blocked.
#[spec("status/blocked/026")]
#[test]
fn status_blocked_026_real_codex_api_failure_ends_error_not_thinking() {
    skip_unless!(common::check_codex_available());

    let command = format!(
        "codex --model {CODEX_REJECTED_MODEL} --sandbox read-only --ask-for-approval never"
    );
    let config_dir = common::harness_tempdir().expect("Codex error new-pane config");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, format!("default_command = {command:?}\n"))
        .expect("write Codex error command");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_imported_codex_credentials()
        .launch_with_fixture("codex-live");
    deck.wait_for_string("No active agents");
    let events = deck.subscribe_events();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("Tab: switch");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        deck.wait_for_grid_string_within(CODEX_COMPOSER_READY, Duration::from_secs(90)),
        "Codex's composer never painted {CODEX_COMPOSER_READY:?}:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"Reply with the word hello.");
    deck.wait_for_string("Reply with the word hello.");
    // Codex drops a submit that lands while it is still initialising, so press
    // Enter until the composer empties — the proof that the turn started
    // (`codex_hooks_001` measured and explains the dropped first Enter).
    let submit_deadline = std::time::Instant::now() + Duration::from_secs(60);
    let submitted = loop {
        deck.send_keys(b"\r");
        if deck.wait_for_grid_string_within(CODEX_COMPOSER_READY, Duration::from_secs(2)) {
            break true;
        }
        if std::time::Instant::now() >= submit_deadline {
            break false;
        }
    };
    assert!(
        submitted,
        "the prompt never left Codex's composer, so no turn started:\n{}",
        deck.snapshot_grid()
    );
    events.wait_for(
        |event| event.agent_type == AgentType::Codex && event.event_type == EventType::Thinking,
        Duration::from_secs(60),
    );
    // The daemon's rollout tailer's event, not `wrap`'s. For THIS failure the
    // stdout classifier emits a Codex `Error` too — measured, ~0.6s after the
    // prompt with the tailer disabled — only because Codex renders the
    // provider's raw JSON body, whose `"type":"error"` is `wrap::CODEX`'s error
    // marker. A failure rendered as plain text matches no marker, and the next
    // non-blank line flips the wrapper back to Working. So this predicate is
    // what proves the fix, and the card check below does not discriminate on
    // its own; `status/blocked/018` asserts the card for a failure with no
    // such output.
    events.wait_for(
        |event| {
            event.agent_type == AgentType::Codex
                && event.event_type == EventType::Error
                && !event.is_wrapper_output_classified()
        },
        Duration::from_secs(90),
    );
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| {
            grid.lines()
                .any(|line| line.contains("Codex") && line.contains("Error"))
        }),
        "the failed Codex turn did not show an Error card:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        !deck.snapshot_grid().contains("Blocked"),
        "a non-quota Codex error must never show Blocked:\n{}",
        deck.snapshot_grid()
    );
}
