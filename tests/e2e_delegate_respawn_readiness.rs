#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! PTY-attached real-agent coverage for PRD #249's `clear = true` delegate path.
//!
//! These tests cover the user-visible happy path that a stand-in cannot: the real deck opens an orchestration tab, a real interactive
//! worker boots in its role pane, the production delegate CLI respawns it, its
//! native hook reports submission and real status, and the worker acts on the
//! delegated task.
//!
//! Both tests are local-only, flaky-tolerant pre-PR e2es. They intentionally
//! have no reel marker because PRD #249 is a bug fix rather than a showcase.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_view.rs"]
mod inbox_view;
#[path = "support/opencode_inbox_live.rs"]
mod opencode_live;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::event::{AgentType, EventType};
use spec::spec;

const ORCH_NAME: &str = "delegate-respawn";
const WORKER_ROLE: &str = "coder";
const DELEGATE_TRIGGER: &str = "delegate-now";
const DELEGATE_TASK_FILE: &str = "delegate-task.md";
const ORCHESTRATOR_SCRIPT: &str = "orchestrator-delegate.sh";
const ORCHESTRATOR_LOG: &str = "orchestrator-delegate.log";
/// Issue #1243: the launcher a [`RealDelegateCase::declared_launcher`] case puts
/// in front of its worker. Its basename names no agent, exactly like `devbox`.
const WORKER_LAUNCHER: &str = "run-worker.sh";

const CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";
const CLAUDE_SENTINEL: &str = "prd249-claude-respawn-4d37c1.txt";
const CLAUDE_SENTINEL_CONTENT: &str = "PRD249_CLAUDE_RESPAWN_OK";

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
    /// Grid substrings meaning "this agent's composer is up and listening",
    /// matched ANY-of: the first one to reach the rendered grid opens the gate.
    ///
    /// **Every member must be free of decorative glyphs, and that is the rule
    /// this field exists to carry rather than a style preference.** Issues #878
    /// / #921: this was a single `&str` pinned to `"Ask anything..."` — three
    /// ASCII periods, bytes `2e 2e 2e` — while OpenCode paints `Ask anything…`
    /// with one U+2026, bytes `e2 80 a6`. The wait underneath is a plain
    /// `contains`, so the two could never match: `/015` burned its full 120 s
    /// and died in this PRECONDITION on a run where OpenCode had booted fine
    /// and was visibly sitting at its composer, having never delegated
    /// anything. Nothing in CI runs this file (lane 2, CLAUDE.md rule 5), so it
    /// stayed red from whenever OpenCode changed that one glyph.
    ///
    /// Swapping in `"Ask anything…"` would have been exactly as fragile — the
    /// next upstream composer touch drifts it again, and the run after that is
    /// another 120 s wait pointing at credentials. So each needle is the part of
    /// the string that carries the MEANING and the decoration is left out of it.
    /// The slice is any-of so a future rendering that is *not* a substring of
    /// the current one can be ADDED here rather than replacing the one that
    /// still works on someone else's version.
    input_ready_needles: &'a [&'a str],
    sentinel_name: &'a str,
    sentinel_content: &'a str,
    /// Issue #1243: `Some(agent)` runs the worker through [`WORKER_LAUNCHER`], a
    /// script that `exec`s the real command, and declares `agent = "<agent>"` on
    /// the role — the `devbox run oc-big` shape this repository's own config was
    /// measured paying the full 30 s readiness timeout under on every delegation,
    /// because the deck could not see the agent behind the launcher. `None`
    /// runs the command directly.
    declared_launcher: Option<&'a str>,
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

fn orchestration_toml(worker_command: &str, declared_agent: Option<&str>) -> String {
    let agent_line = declared_agent
        .map(|agent| format!("agent = {agent:?}\n"))
        .unwrap_or_default();
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
         {agent_line}\
         clear = true\n"
    )
}

/// The sentinel is named by its absolute path: measured on 2026-10-03, a real
/// OpenCode worker on a starved box ran the requested `printf` in the parent of
/// its working directory, so "the current working directory" left the model a
/// choice the assertion does not.
fn delegate_task(case: &RealDelegateCase<'_>, work: &Path) -> String {
    let path = work.join(case.sentinel_name);
    format!(
        "Create the file {path} with the exact contents {contents} and no trailing newline. \
         Use the shell to run exactly this command: printf '{contents}' > {path}. Do not \
         modify any other file. That is the entire task.",
        path = path.display(),
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

fn run_real_clear_true_delegate(deck: &TuiDeck, worker_command: &str, case: RealDelegateCase<'_>) {
    deck.wait_for_string("No active agents");

    let work = deck.workdir().to_path_buf();
    let role_command = match case.declared_launcher {
        Some(_) => {
            write_executable(
                &work.join(WORKER_LAUNCHER),
                &format!("#!/bin/sh\nexec {worker_command}\n"),
            );
            let launcher = format!("./{WORKER_LAUNCHER}");
            assert_eq!(
                AgentType::from_command(Some(&launcher)),
                None,
                "control: the launcher must hide the agent from command inference, or this is \
                 not the configuration issue #1243 measured"
            );
            launcher
        }
        None => worker_command.to_string(),
    };
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        orchestration_toml(&role_command, case.declared_launcher),
    )
    .expect("write delegate orchestration config");
    std::fs::write(work.join(DELEGATE_TASK_FILE), delegate_task(&case, &work))
        .expect("write delegated task body");
    write_executable(&work.join(ORCHESTRATOR_SCRIPT), ORCHESTRATOR_BODY);

    let events = deck.subscribe_events();
    open_orchestration(deck);
    deck.wait_for_string(WORKER_ROLE);

    // The orchestration opens focused on its start role. Detach, then jump to
    // the second role card so the real worker TUI itself is visibly on screen.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    deck.send_bytes(b"2");
    // Any-of over `input_ready_needles`, via the existing grid-predicate helper
    // so this stays in the test file rather than widening shared harness surface.
    let needles = case.input_ready_needles;
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(120), |grid| needles
            .iter()
            .any(|needle| grid.contains(needle))),
        "the REAL interactive {name} worker never showed an input-ready marker within 120 s \
         (none of {needles:?} reached the rendered grid), so this run stopped in a PRECONDITION \
         and never delegated anything.\n\
         \n\
         Two unrelated causes land on this line and the grid below tells them apart — read it \
         before concluding either (issues #878/#921 were misdiagnosed for exactly this reason, \
         because this message used to assert the first cause outright):\n\
         \n\
         * The grid carries no {name} UI at all, or a CLI/auth/model error: the agent never \
         booted. Credentials, the CLI on PATH, or an unreachable model.\n\
         * The grid shows a live {name} UI sitting at its composer: the agent is FINE and the \
         marker has drifted upstream. Read `RealDelegateCase::input_ready_needles`' doc comment \
         before touching it — the repair is not to paste in whatever glyph you now see.\n\
         \n\
         Final grid:\n{grid}",
        name = case.agent_name,
        needles = needles,
        grid = deck.snapshot_grid()
    );

    // Return to the role-card surface before releasing the orchestrator. The
    // status sequence below is captured only from this point forward, so it is
    // the delegated turn rather than boot-time card paint.
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    // Issue #243: stamped so the submission wait below can be scoped to events
    // broadcast AFTER the delegate, and so the latency bound has an anchor. The
    // FIRST worker has been up for a while by now and `EventSub::wait_for` scans
    // everything collected since the subscription opened, so an unscoped
    // predicate could match a pre-delegate event and measure nonsense.
    let delegate_released_at = chrono::Utc::now();
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
                && event.timestamp >= delegate_released_at
                && event.user_prompt.as_deref().is_some_and(|prompt| {
                    if case.agent_type == AgentType::ClaudeCode {
                        prompt.starts_with("✉ ")
                    } else {
                        prompt.contains(&format!("worker-task-{WORKER_ROLE}.md"))
                    }
                })
        },
        Duration::from_secs(90),
    );
    assert!(
        submitted.user_prompt.as_deref().is_some_and(|prompt| {
            if case.agent_type == AgentType::ClaudeCode {
                prompt.starts_with("✉ ")
            } else {
                prompt.contains(&format!("worker-task-{WORKER_ROLE}.md"))
            }
        }),
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
    if case.agent_type == AgentType::ClaudeCode {
        let agent = common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|r| {
                matches!(&r.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == WORKER_ROLE)
            })
            .expect("replacement Claude worker");
        let messages = inbox_view::messages(
            deck.attach_socket_path(),
            agent.pane_id_env.as_deref().unwrap(),
        );
        let task = messages
            .iter()
            .find(|m| {
                m.text
                    .as_deref()
                    .is_some_and(|s| s.contains(case.sentinel_name))
            })
            .expect("delegated task in replacement inbox");
        assert_eq!(task.bind_agent.as_deref(), Some(agent.id.as_str()));
        assert_eq!(task.deliveries, 1);
        assert_eq!(task.state, dot_agent_deck::inbox::StateName::Acknowledged);
        assert!(
            !agent
                .live
                .unwrap()
                .last_user_prompt
                .unwrap_or_default()
                .contains(case.sentinel_name)
        );
    }
}

/// Scenario: Open an orchestration through the real PTY-attached deck with a `clear = true` worker running interactive Haiku, wait for its prompt editor and release the real delegate CLI. The replacement worker must consume one inbox task, visibly traverse Thinking and Working with Bash, create the unique sentinel and acknowledge the generation-bound message while its card carries an envelope label.
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
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .launch_with_fixture("minimal");

    run_real_clear_true_delegate(
        &deck,
        &worker_command,
        RealDelegateCase {
            agent_name: "Claude Code",
            agent_type: AgentType::ClaudeCode,
            input_ready_needles: &["? for shortcuts"],
            sentinel_name: CLAUDE_SENTINEL,
            sentinel_content: CLAUDE_SENTINEL_CONTENT,
            declared_launcher: None,
        },
    );
}

/// Scenario: Run interactive OpenCode through a launcher declared as OpenCode and delegate with clear enabled. The replacement must visibly run the framed inbox task, write the exact sentinel and acknowledge native delivery without a typed task pointer.
#[spec("orchestration/delegate/015")]
#[test]
#[cfg(unix)]
fn delegate_015_real_opencode_worker_acts_on_clear_true_delegate() {
    skip_unless!(opencode_live::available());
    let mut op = opencode_live::OpenCode::with_clear(false, 0, true, true, true);
    let name = "opencode-clear-8a62f4.txt";
    let body = format!(
        "Use the shell to run exactly: printf '{}' > '{}'. Then reply DONE-{name} and end your turn.",
        opencode_live::CONTENT,
        op.work.join(name).display()
    );
    let id = op.clear_delegate(&body);
    op.done(&id, name);
    assert!(!op.log().contains("doorbell rang"), "{}", op.evidence());
}

/// Scenario: Hold a real OpenCode replacement in its launcher while a genuine clear delegate queues its task. Release boot and require the same replacement to consume that inbox task once, visibly reply, write the sentinel and acknowledge it without a retype or another respawn.
#[spec("orchestration/delegate/046")]
#[test]
#[cfg(unix)]
fn delegate_046_real_opencode_consumes_boot_queued_task_in_place() {
    skip_unless!(opencode_live::available());
    let mut op = opencode_live::OpenCode::with_clear(false, 0, true, true, true);
    let script = op.work.join("run-worker.sh");
    std::fs::write(&script, format!("#!/bin/sh\nwhile [ ! -f boot-release ]; do sleep 0.1; done\nexec opencode --model {} --auto\n", common::opencode_test_model())).unwrap();
    let name = "opencode-boot-6d3e9a.txt";
    let body = format!(
        "Use the shell to run exactly: printf '{}' > '{}'. Then reply DONE-{name} and end your turn.",
        opencode_live::CONTENT,
        op.work.join(name).display()
    );
    let id = op.clear_delegate(&body);
    let replacement = op.pane.agent.clone();
    inbox::state(&op.deck, &op.pane, &id, "queued");
    std::fs::write(op.work.join("boot-release"), "").unwrap();
    op.done(&id, name);
    assert_eq!(
        op.record()["id"],
        replacement,
        "the queued task must run in the first replacement"
    );
    let listing = inbox::listing(&op.deck, &op.pane);
    let message = listing["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap();
    assert_eq!(message["deliveries"], 1, "one native hand-over after boot");
}
