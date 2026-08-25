#![cfg(feature = "e2e")]

//! PTY-attached real-agent coverage for PRD #249's `clear = true` delegate path.
//!
//! The deterministic slow-readiness stand-in in `orchestration/delegate/012`
//! pins the race itself. These tests cover the user-visible happy path that a
//! stand-in cannot: the real deck opens an orchestration tab, a real interactive
//! worker boots in its role pane, the production delegate CLI respawns it, its
//! native hook reports submission and real status, and the worker acts on the
//! delegated task.
//!
//! Both tests are local-only, flaky-tolerant pre-PR e2es. They intentionally
//! have no reel marker because PRD #249 is a bug fix rather than a showcase.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{TuiDeck, TuiDeckBuilder};
use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::state::DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS;
use spec::spec;

const ORCH_NAME: &str = "delegate-respawn";
const WORKER_ROLE: &str = "coder";
const DELEGATE_TRIGGER: &str = "delegate-now";
const DELEGATE_TASK_FILE: &str = "delegate-task.md";
const ORCHESTRATOR_SCRIPT: &str = "orchestrator-delegate.sh";
const ORCHESTRATOR_LOG: &str = "orchestrator-delegate.log";

const CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";
const CLAUDE_SENTINEL: &str = "prd249-claude-respawn-4d37c1.txt";
const CLAUDE_SENTINEL_CONTENT: &str = "PRD249_CLAUDE_RESPAWN_OK";

const OPENCODE_SENTINEL: &str = "prd249-opencode-respawn-8a62f4.txt";
const OPENCODE_SENTINEL_CONTENT: &str = "PRD249_OPENCODE_RESPAWN_OK";

/// Test-only forwarding seam for the M2 observation run. `TuiDeck` scrubs the
/// host environment, so setting this variable on `cargo test-e2e delegate_015`
/// explicitly maps its value to the production readiness-buffer variable in the
/// spawned deck and daemon. Normal runs leave it unset and exercise the default.
const E2E_READINESS_BUFFER_OVERRIDE: &str = "DOT_AGENT_DECK_E2E_DELEGATE_READINESS_BUFFER_MS";

const ORCHESTRATOR_BODY: &str = r#"#!/bin/sh
while [ ! -f delegate-now ]; do sleep 0.2; done
dot-agent-deck delegate --to coder --task-file delegate-task.md \
    >> orchestrator-delegate.log 2>&1
printf 'delegate exit=%s\n' "$?" >> orchestrator-delegate.log
exec cat > /dev/null
"#;

struct RealDelegateCase<'a> {
    agent_name: &'a str,
    agent_type: AgentType,
    input_ready_needle: &'a str,
    sentinel_name: &'a str,
    sentinel_content: &'a str,
}

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write orchestrator role script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod orchestrator role script");
}

fn orchestration_toml(worker_command: &str) -> String {
    format!(
        "[[orchestrations]]\n\
         name = \"{ORCH_NAME}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"./{ORCHESTRATOR_SCRIPT}\"\n\
         start = true\n\n\
         [[orchestrations.roles]]\n\
         name = \"{WORKER_ROLE}\"\n\
         command = {worker_command:?}\n\
         clear = true\n"
    )
}

fn delegate_task(case: &RealDelegateCase<'_>) -> String {
    format!(
        "Create the file {name} in the current working directory with the exact contents \
         {contents} and no trailing newline. Use the shell to run exactly this command: \
         printf '{contents}' > {name}. Do not modify any other file. That is the entire task.",
        name = case.sentinel_name,
        contents = case.sentinel_content,
    )
}

fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
}

fn maybe_forward_readiness_override(builder: TuiDeckBuilder) -> TuiDeckBuilder {
    match std::env::var(E2E_READINESS_BUFFER_OVERRIDE) {
        Ok(value) => builder.with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, value),
        Err(_) => builder,
    }
}

fn run_real_clear_true_delegate(deck: TuiDeck, worker_command: &str, case: RealDelegateCase<'_>) {
    deck.wait_for_string("No active sessions");

    let work = deck.workdir().to_path_buf();
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        orchestration_toml(worker_command),
    )
    .expect("write delegate orchestration config");
    std::fs::write(work.join(DELEGATE_TASK_FILE), delegate_task(&case))
        .expect("write delegated task body");
    write_executable(&work.join(ORCHESTRATOR_SCRIPT), ORCHESTRATOR_BODY);

    let events = deck.subscribe_events();
    open_orchestration(&deck);
    deck.wait_for_string(WORKER_ROLE);

    // The orchestration opens focused on its start role. Detach, then jump to
    // the second role card so the real worker TUI itself is visibly on screen.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    deck.send_bytes(b"2");
    assert!(
        deck.wait_for_grid_string_within(case.input_ready_needle, Duration::from_secs(120)),
        "the REAL interactive {} worker never became visibly input-ready (missing {:?}) \
         before delegation; this is a boot/auth failure, not a delegate failure.\nFinal grid:\n{}",
        case.agent_name,
        case.input_ready_needle,
        deck.snapshot_grid()
    );

    // Return to the role-card surface before releasing the orchestrator. The
    // status sequence below is captured only from this point forward, so it is
    // the delegated turn rather than boot-time card paint.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    std::fs::write(work.join(DELEGATE_TRIGGER), "").expect("release delegate trigger");

    // Real native status on the user-visible card: prompt submission enters
    // Thinking, the requested shell operation enters Working, and its tool name
    // renders. This rolling-stream wait starts after the trigger, so old frames
    // cannot satisfy it.
    deck.wait_for_strings_in_order_then_any_within(
        &["Thinking", "Working"],
        &["Bash", "bash"],
        Duration::from_secs(120),
    );

    // Native hook proof that the pointer was submitted inside the replacement
    // agent, rather than merely echoed into a booting PTY. Both Claude Code's
    // UserPromptSubmit hook and OpenCode's session.prompt event populate this
    // field with the actual submitted text.
    let submitted = events.wait_for(
        |event| {
            event.event_type == EventType::Thinking
                && event.agent_type == case.agent_type
                && event
                    .user_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt.contains(&format!("worker-task-{WORKER_ROLE}.md")))
        },
        Duration::from_secs(90),
    );
    assert!(
        submitted
            .user_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains(&format!("worker-task-{WORKER_ROLE}.md"))),
        "the REAL {} worker emitted a prompt event, but it was not the delegated task pointer: {:?}",
        case.agent_name,
        submitted.user_prompt
    );

    let sentinel = work.join(case.sentinel_name);
    if let Err(observed) =
        common::wait_for_file_trimmed_eq(&sentinel, case.sentinel_content, Duration::from_secs(90))
    {
        let task_pointer = work
            .join(".dot-agent-deck")
            .join(format!("worker-task-{WORKER_ROLE}.md"));
        let orchestrator_log = std::fs::read_to_string(work.join(ORCHESTRATOR_LOG))
            .unwrap_or_else(|error| format!("<unreadable: {error}>"));
        panic!(
            "the REAL {} worker never created {:?} with exact contents {:?}; observed: {}; \
             task_pointer_written={} orchestrator_log={:?}\nFinal grid:\n{}",
            case.agent_name,
            case.sentinel_name,
            case.sentinel_content,
            observed,
            task_pointer.exists(),
            orchestrator_log,
            deck.snapshot_grid()
        );
    }
}

/// Scenario: Open an orchestration through the real PTY-attached deck with a `clear = true` worker running interactive Claude Code on Haiku, visibly wait for its prompt editor, and release a script that invokes the real delegate CLI. The replacement worker must submit its task pointer, visibly traverse Thinking and Working with Bash, and create the uniquely named sentinel requested by the delegated task.
#[spec("orchestration/delegate/014")]
#[test]
#[cfg(unix)]
fn delegate_014_real_claude_worker_acts_on_clear_true_delegate() {
    skip_unless!(common::check_claude_available());

    // `Write` is here for the footer, not the assertion (#303). The sentinel is
    // created with Bash, so this test would still pass without it — but the task
    // file's `## When done` footer then sends the worker into a `Write` approval
    // prompt it cannot answer, and this case is `[reel]`-marked, so that dialog
    // would be the last thing on its recorded cast.
    let worker_command = format!("claude --model {CLAUDE_MODEL} --allowedTools Bash Read Write");
    let deck = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, "1000")
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .launch_with_fixture("minimal");

    run_real_clear_true_delegate(
        deck,
        &worker_command,
        RealDelegateCase {
            agent_name: "Claude Code",
            agent_type: AgentType::ClaudeCode,
            input_ready_needle: "? for shortcuts",
            sentinel_name: CLAUDE_SENTINEL,
            sentinel_content: CLAUDE_SENTINEL_CONTENT,
        },
    );
}

/// Scenario: Open an orchestration through the real PTY-attached deck with a `clear = true` worker running interactive OpenCode on a cheap mini model, visibly wait for its TUI, and release a script that invokes the real delegate CLI. The replacement worker must submit its task pointer, visibly traverse Thinking and Working with its shell tool, and create the uniquely named sentinel requested by the delegated task; a test-only env seam permits the same scenario to be observed with the readiness buffer set to zero.
#[spec("orchestration/delegate/015")]
#[test]
#[cfg(unix)]
fn delegate_015_real_opencode_worker_acts_on_clear_true_delegate() {
    skip_unless!(common::check_opencode_available());

    let worker_command = format!("opencode --model {} --auto", common::opencode_test_model());
    let builder = TuiDeck::builder()
        .with_pty_size(180, 45)
        .with_env("PATH", path_with_binary_dir())
        .with_env(DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS, "1000")
        .with_imported_opencode_credentials();
    let deck = maybe_forward_readiness_override(builder).launch_with_fixture("minimal");

    run_real_clear_true_delegate(
        deck,
        &worker_command,
        RealDelegateCase {
            agent_name: "OpenCode",
            agent_type: AgentType::OpenCode,
            input_ready_needle: "Ask anything...",
            sentinel_name: OPENCODE_SENTINEL,
            sentinel_content: OPENCODE_SENTINEL_CONTENT,
        },
    );
}
