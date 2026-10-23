#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Issue #1094: real-agent coverage for the `pane restart <role>` and `pane
//! spawn <role>` verbs (issue #868), which every other test drives with a `cat`
//! stand-in (`tests/e2e_pane_restart_live.rs`, `tests/e2e_pane_spawn_live.rs`).
//!
//! Both tests open an orchestration in the real PTY-attached deck, bring up an
//! interactive Claude Code worker on Haiku through the verb under test, and
//! then hand it a task with the real `delegate` CLI, the way an orchestrator
//! would. The task writes a uniquely named sentinel file, so the assertion does
//! not depend on how the model phrases its reply. A stand-in carries no agent
//! identity and does no work, so neither "the pane the verb produced is a
//! recognized agent" nor "it works" can be observed without one.
//!
//! Lane 2: local-only and flaky-tolerant (real LLM). Run with
//! `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test-e2e-live -E
//! 'binary(e2e_pane_verbs_real_agent)'` so a preflight that cannot be satisfied
//! fails instead of skipping.

mod common;
#[path = "support/inbox_view.rs"]
mod inbox_view;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::event::AgentType;
use spec::spec;

const CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";
const ORCH_NAME: &str = "pane-verbs-real";
/// Claude Code's composer hint: once it is drawn, Claude is listening for input.
const CLAUDE_READY: &str = "? for shortcuts";
const SENTINEL_CONTENT: &str = "PANE_VERBS_REAL_AGENT_OK";

/// `Write` is allowed for the delegated task's `## When done` footer, not for
/// the sentinel (Bash writes that): without it the worker ends its turn in an
/// approval prompt nobody answers, which would be the last frame of the cast.
fn claude_command() -> String {
    format!("claude --model {CLAUDE_MODEL} --allowedTools Bash Read Write")
}

/// One orchestration: a `cat` orchestrator (the test plays the orchestrator by
/// running the CLI with its pane id) and the `extra_roles` after it. Workers
/// are `clear = false` so a delegate reaches the generation the verb produced
/// instead of replacing it.
fn orchestration_toml(extra_roles: &[(&str, &str)]) -> String {
    let mut toml = format!(
        "[[orchestrations]]\n\
         name = \"{ORCH_NAME}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"cat\"\n\
         start = true\n"
    );
    for (name, command) in extra_roles {
        toml.push_str(&format!(
            "\n[[orchestrations.roles]]\n\
             name = \"{name}\"\n\
             command = {command:?}\n\
             clear = false\n"
        ));
    }
    toml
}

/// The new-pane dialog, opening the workdir's single orchestration: Right
/// selects `[Orch: …]`, which hides the Command field, so a second Enter submits.
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

/// Command mode, then a digit: jump to that role card and focus its pane. Only
/// the first digit after `Ctrl+D` is a jump, because a successful focus returns
/// to PaneInput, where a second digit would be typed into the agent.
fn focus_role_card(deck: &TuiDeck, digit: u8) {
    deck.send_bytes(b"\x04");
    deck.wait_for_absence("[Command Mode Ctrl+D]");
    deck.send_bytes(&[digit]);
    deck.wait_for_string("[Command Mode Ctrl+D]");
}

/// The daemon's record for `role` in this orchestration, if one is listed.
fn role_record(deck: &TuiDeck, role: &str) -> Option<dot_agent_deck::agent_pty::AgentRecord> {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|r| {
            matches!(
                &r.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
            )
        })
}

fn orchestrator_pane_id(deck: &TuiDeck) -> String {
    role_record(deck, "orchestrator")
        .and_then(|r| r.pane_id_env)
        .expect("the orchestrator role must be listed with a DOT_AGENT_DECK_PANE_ID")
}

/// Wait for the attached TUI to draw Claude's composer in the focused pane.
/// The rendered grid, not the pane's raw bytes: Claude repaints its footer in
/// place, so the bytes need not hold the hint whole (one run's held `forshortuts`).
fn wait_for_claude_composer(deck: &TuiDeck, what: &str) {
    eprintln!("pane-verbs: waiting for Claude's composer ({what})");
    assert!(
        deck.wait_for_grid_string_within(
            CLAUDE_READY,
            common::load_scaled(Duration::from_secs(90))
        ),
        "{what}: the attached TUI never drew an interactive Claude Code composer \
         ({CLAUDE_READY:?}). No Claude UI on the grid below means it never booted \
         (credentials, `claude` on PATH, the model); a Claude UI sitting at its composer \
         means the hint text drifted upstream.\nGrid:\n{}",
        deck.snapshot_grid()
    );
}

/// The real `delegate` CLI, run with the orchestrator's identity.
fn delegate_cli(deck: &TuiDeck, caller_pane: &str, to: &str, task: &str) {
    let output = std::process::Command::new(common::contained_deck_binary())
        .args(["delegate", "--to", to, "--task", task])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", caller_pane)
        .output()
        .expect("run the real `dot-agent-deck delegate` CLI");
    assert!(
        output.status.success(),
        "`delegate --to {to}` must succeed -- stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A directive task naming the sentinel by absolute path, so the model has no
/// choice of directory to make.
fn sentinel_task(sentinel: &Path) -> String {
    format!(
        "Use the Bash tool to run exactly this command: printf '{SENTINEL_CONTENT}' > '{path}'. \
         Do not modify any other file. That is the entire task.",
        path = sentinel.display(),
    )
}

/// Delegate the sentinel task to `role`, whose live generation is `agent_id`,
/// and require the generation to do it: the file appears with the exact
/// contents, the attached TUI paints the worker visibly working on it, and the
/// one inbox message carrying the task is bound to that generation.
fn delegate_and_require_work(deck: &TuiDeck, role: &str, agent_id: &str, sentinel_name: &str) {
    let sentinel = deck.workdir().join(sentinel_name);
    let caller = orchestrator_pane_id(deck);
    let painted_from = deck.stream_text().len();
    delegate_cli(deck, &caller, role, &sentinel_task(&sentinel));

    if let Err(observed) = common::wait_for_file_trimmed_eq(
        &sentinel,
        SENTINEL_CONTENT,
        common::load_scaled(Duration::from_secs(120)),
    ) {
        panic!(
            "the real Claude Code {role} never created {sentinel_name:?} with exact contents \
             {SENTINEL_CONTENT:?}; observed: {observed}\nGrid:\n{}",
            deck.snapshot_grid()
        );
    }

    // The status the user reads: the deck painted the worker going Thinking,
    // Working and back to Idle after the delegate, not a frame from before it.
    // Idle comes after the task's work-done footer, so the turn is over.
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(90)), || {
            let stream = deck.stream_text();
            let turn = stream.get(painted_from..).unwrap_or_default();
            common::match_needles_in_order(turn.as_bytes(), &["Thinking", "Working", "Idle"]) == 3
        }),
        "the attached TUI never painted {role} going Thinking -> Working -> Idle for the \
         delegated task.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    let pane = role_record(deck, role)
        .and_then(|r| r.pane_id_env)
        .expect("the worker is still listed with its pane id");
    let bound: Vec<_> = inbox_view::messages(deck.attach_socket_path(), &pane)
        .into_iter()
        .filter(|m| m.text.as_deref().is_some_and(|t| t.contains(sentinel_name)))
        .collect();
    assert!(
        bound.len() == 1 && bound[0].bind_agent.as_deref() == Some(agent_id),
        "the delegated task must be one inbox message bound to {role}'s generation \
         {agent_id}; matching messages = {bound:?}"
    );
}

fn launch_deck() -> TuiDeck {
    TuiDeck::builder()
        // The test runs the orchestrator's CLI calls from outside its pane,
        // so it holds no hook capability token for that pane.
        .impersonating_pane_signals()
        .with_pty_size(180, 45)
        // The worker's task footer runs `dot-agent-deck work-done`.
        .with_env("PATH", common::path_with_binary_dir())
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        // The waits' own bounds, the harness's fixed 30s ones included, sum
        // to about 730s on a quiet box, past the harness daemon's default 300s
        // lifetime; 900s keeps the daemon up until a stalled phase has
        // reported (`.config/nextest.toml`).
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "900")
        .launch_with_fixture("minimal")
}

/// Scenario: Open an orchestration whose `coder` is interactive Claude Code on Haiku, focus coder's pane and quit Claude there with Ctrl+C twice, so the worker exits without reporting work-done. Run the real `pane restart coder` CLI from the orchestrator's identity until the daemon accepts it, then delegate a task that writes a uniquely named sentinel. A new Claude Code generation must come up in the same pane, show its composer, write the sentinel, and be painted Thinking, Working and then Idle in the still-attached TUI, with the task bound to that new generation.
#[spec("pane/real-agent/001")]
#[test]
fn real_agent_001_restart_brings_back_a_working_claude() {
    skip_unless!(common::check_claude_available());
    let deck = launch_deck();
    deck.wait_for_string("No active agents");
    std::fs::write(
        deck.workdir().join(".dot-agent-deck.toml"),
        orchestration_toml(&[("coder", &claude_command())]),
    )
    .expect("write the orchestration config");

    open_orchestration(&deck);
    deck.wait_for_string("coder");
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(30)), || {
            role_record(&deck, "coder").is_some()
        }),
        "coder must be listed once the orchestration opens"
    );
    let first = role_record(&deck, "coder").expect("coder is listed");
    let coder_pane = first.pane_id_env.clone().expect("coder carries a pane id");

    focus_role_card(&deck, b'2');
    wait_for_claude_composer(&deck, "before the restart");

    // Quit Claude the way a user does. The first Ctrl+C on an empty composer
    // arms the exit and the second one confirms it.
    deck.send_bytes(b"\x03");
    assert!(
        deck.wait_for_grid_string_within("again to exit", Duration::from_secs(10)),
        "Claude never asked for the second Ctrl+C.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    deck.send_bytes(b"\x03");
    // Claude takes its composer down as it exits, so from here on only the
    // restarted generation can draw the hint again.
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| !grid
            .contains(CLAUDE_READY)),
        "the first Claude never took its composer down after the second Ctrl+C.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // No `--force`: the daemon refuses until it has seen the worker exit, and
    // the helper retries exactly that refusal.
    common::retry_pane_restart_until_success(
        deck.hook_socket_path(),
        &orchestrator_pane_id(&deck),
        "coder",
        common::load_scaled(Duration::from_secs(30)),
    );
    let restarted = || role_record(&deck, "coder").filter(|r| r.id != first.id);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(30)), || {
            restarted().is_some()
        }),
        "`pane restart coder` succeeded but no new coder generation was listed"
    );
    let second = restarted().expect("a new generation was just listed");
    assert_eq!(
        second.pane_id_env.as_deref(),
        Some(coder_pane.as_str()),
        "a restart keeps the role's pane"
    );
    assert_eq!(
        second.agent_type,
        Some(AgentType::ClaudeCode),
        "the restarted generation must be recognized as Claude Code"
    );

    wait_for_claude_composer(&deck, "after the restart");
    delegate_and_require_work(&deck, "coder", &second.id, "pane-restart-real-7f31c2.txt");
}

/// Scenario: Open an orchestration with a `cat` orchestrator and a `cat` coder, then add a `reviewer` role running interactive Claude Code on Haiku to the running orchestration's config and run the real `pane spawn reviewer` CLI from the orchestrator's identity. Reviewer's card must join the already-open tab, and focusing it must show Claude's composer. A delegated task that writes a uniquely named sentinel must then be done by that reviewer and painted Thinking, Working and then Idle in the attached TUI, with the task bound to the spawned generation.
#[spec("pane/real-agent/002")]
#[test]
fn real_agent_002_spawn_adds_a_working_claude_to_the_open_tab() {
    skip_unless!(common::check_claude_available());
    let deck = launch_deck();
    deck.wait_for_string("No active agents");
    let config = deck.workdir().join(".dot-agent-deck.toml");
    std::fs::write(&config, orchestration_toml(&[("coder", "cat")]))
        .expect("write the orchestration config");

    open_orchestration(&deck);
    deck.wait_for_string("coder");
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(30)), || {
            role_record(&deck, "coder").is_some()
        }),
        "coder must be listed once the orchestration opens"
    );
    assert!(
        role_record(&deck, "reviewer").is_none(),
        "precondition: reviewer is not part of the config the tab opened from"
    );

    std::fs::write(
        &config,
        orchestration_toml(&[("coder", "cat"), ("reviewer", &claude_command())]),
    )
    .expect("add reviewer to the running orchestration's config");
    let output = std::process::Command::new(common::contained_deck_binary())
        .args(["pane", "spawn", "reviewer"])
        .env("DOT_AGENT_DECK_PANE_ID", orchestrator_pane_id(&deck))
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .output()
        .expect("run the real `dot-agent-deck pane spawn reviewer` CLI");
    assert!(
        output.status.success(),
        "`pane spawn reviewer` must succeed -- stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(30)), || {
            role_record(&deck, "reviewer").is_some()
        }),
        "`pane spawn reviewer` succeeded but reviewer was never listed"
    );
    let reviewer = role_record(&deck, "reviewer").expect("reviewer is listed");
    assert_eq!(
        reviewer.agent_type,
        Some(AgentType::ClaudeCode),
        "the spawned reviewer must be recognized as Claude Code"
    );
    assert!(
        deck.wait_for_grid_string_within("reviewer", Duration::from_secs(15)),
        "reviewer's card never joined the open orchestration tab.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Reviewer is the third role in the config, so its card is the third one.
    focus_role_card(&deck, b'3');
    wait_for_claude_composer(&deck, "after the spawn");
    delegate_and_require_work(
        &deck,
        "reviewer",
        &reviewer.id,
        "pane-spawn-real-2b9e54.txt",
    );
}
