#![cfg(all(feature = "e2e", unix))]

//! Issue #2087: which skill an agent loaded. Claude Code's own telemetry names
//! every project skill `custom_skill`, so the deck records the name itself, on
//! the run, from the `Skill` tool's `PreToolUse` hook — and only for a skill
//! defined in a skill directory on disk, never the tool's arguments.

dot_agent_deck_test_harness::link!();
#[allow(dead_code)] // This target reads the run span and raw bodies only.
mod telemetry_operation_receiver;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::{Value, json};
use spec::spec;
use std::io::Write;
use std::process::{Command, Stdio};
use telemetry_operation_receiver::{EXPORT_WAIT, Receiver, attribute};

/// A skill the fixture defines in its project's `.claude/skills`.
const DEFINED: &str = "dad-skill-2087-defined";
/// A name the agent asks for that no skill directory holds.
const UNDEFINED: &str = "dad-skill-2087-undefined";
/// The `Skill` tool's free-text arguments, which must never leave the box.
const ARGS: &str = "PRIVATE_SKILL_ARGS_2087";

/// Scenario: Define a skill in the stand-in pane's project `.claude/skills`, then send three Claude Code `Skill` tool calls through the real hook CLI: that skill with free-text arguments, a name no skill directory holds, and a path-shaped name. Close the pane: its exported run carries one `deck.skill` event per call, naming only the defined skill, counts three, and no request body holds the arguments, the undefined name or the path.
#[spec("telemetry/otlp/048")]
#[test]
fn telemetry_otlp_048_a_skill_call_names_a_defined_skill_only() {
    let receiver = Receiver::start();
    let deck = TuiDeck::builder()
        .without_agent_credentials()
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        .with_env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .with_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .with_continue_session(
            "skill-standin",
            "sh -c 'echo SKILL_STANDIN_READY; exec cat'",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("SKILL_STANDIN_READY");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("Dir:");
    let agent = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .next()
        .unwrap();
    let pane = agent.pane_id_env.as_deref().unwrap();
    let skill_dir = deck.workdir().join(".claude/skills").join(DEFINED);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "---\nname: x\n---\n").unwrap();

    let hook = |call: &str, input: Value| {
        let payload = json!({
            "session_id": "skill-synthetic", "hook_event_name": "PreToolUse",
            "tool_name": "Skill", "tool_use_id": call,
            "tool_input": input, "cwd": deck.workdir()
        });
        let mut child = Command::new(common::contained_deck_binary())
            .args(["hook", "--agent", "claude-code"])
            .env_clear()
            .env("HOME", deck.home_dir())
            .env("PATH", "/usr/bin:/bin")
            .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
            .env("DOT_AGENT_DECK_PANE_ID", pane)
            .env("DOT_AGENT_DECK_AGENT_ID", &agent.id)
            .env("OTEL_SDK_DISABLED", "true")
            .current_dir(deck.workdir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{call} CLI failed: {output:?}");
    };

    hook("defined", json!({"skill": DEFINED, "args": ARGS}));
    hook("undefined", json!({"skill": UNDEFINED}));
    hook("path", json!({"skill": "../../skills/dad-skill-2087-path"}));
    deck.wait_for_string("Working");

    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
            keep_worktree: false,
        },
    )
    .unwrap();
    assert!(response.ok, "stop pane: {response:?}");
    assert!(
        common::wait_until(EXPORT_WAIT, || receiver.run(pane).is_some()),
        "run never exported"
    );
    let run = receiver.run(pane).unwrap();
    let skills: Vec<Option<&str>> = run["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["name"] == "deck.skill")
        .map(|e| attribute(e, "deck.skill.name"))
        .collect();
    assert_eq!(
        skills,
        vec![Some(DEFINED), None, None],
        "one deck.skill event per Skill call, naming only the defined skill (issue #2087): {run}"
    );
    let count = run["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "deck.skills.count")
        .map(|a| a["value"]["intValue"].clone());
    assert_eq!(count, Some(json!("3")), "{run}");
    for body in receiver.bodies() {
        let body = String::from_utf8_lossy(&body);
        for private in [ARGS, UNDEFINED, "dad-skill-2087-path"] {
            assert!(!body.contains(private), "{private} reached OTLP: {body}");
        }
    }
}
