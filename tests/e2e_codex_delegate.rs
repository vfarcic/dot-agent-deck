#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! A real clear=true Codex worker receives a delegate through its inbox.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_view.rs"]
mod inbox_view;
#[path = "support/codex_inbox_live.rs"]
mod live;

use common::TuiDeck;
use serde_json::json;
use spec::spec;
use std::time::Duration;

const SENTINEL: &str = "codex-delegate-6f21ba.txt";
const CONTENT: &str = "CODEX_DELEGATE_OK";

/// Scenario: Open an orchestration with an interactive Codex worker and run the real delegate CLI from its orchestrator role. The clear=true replacement must receive the inbox doorbell and submit-hook injection, visibly work with an envelope card, acknowledge the task and create the sentinel.
#[spec("orchestration/delegate/009")]
#[test]
fn delegate_009_real_codex_worker_acts_on_clear_true_delegate() {
    skip_unless!(common::check_codex_available());
    let path = common::path_with_binary_dir();
    let deck = TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_codex_credentials()
        .with_env("PATH", path)
        .with_env("DOT_AGENT_DECK_LOG", "codex-delegate.log")
        .launch_with_fixture("minimal");
    assert!(deck.wait_for_grid_string_within("No active agents", live::BUDGET));
    live::prepare(&deck);
    let command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model()
    );
    std::fs::write(deck.workdir().join(".dot-agent-deck.toml"), format!(
        "[[orchestrations]]\nname = \"codex-delegate\"\n\n[[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./delegate.sh\"\nstart = true\n\n[[orchestrations.roles]]\nname = \"coder\"\ncommand = {command:?}\nclear = true\n"
    )).unwrap();
    std::fs::write(deck.workdir().join("delegate-task.md"),format!(
        "Run exactly this shell command: printf '{CONTENT}' > '{}'. Then reply DONE and end your turn.",
        // PRD #1666: the absolute path, not "the current directory".
        deck.workdir().join(SENTINEL).display()
    )).unwrap();
    let script = deck.workdir().join("delegate.sh");
    std::fs::write(&script, "#!/bin/sh\nwhile [ ! -f delegate-now ]; do sleep 0.2; done\ndot-agent-deck delegate --to coder --task-file delegate-task.md > delegate-cli.log 2>&1\nexec cat\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("coder");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    deck.send_bytes(b"2");
    assert!(
        deck.wait_for_grid_predicate_within(live::BUDGET, |grid| grid
            .to_ascii_lowercase()
            .contains(&common::codex_test_model().to_ascii_lowercase())),
        "real Codex must boot; {}",
        deck.snapshot_grid()
    );
    let before = inbox::attach(&deck, json!({"op":"list-agents"}));
    let original = before["agent_records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tab_membership"]["role_name"] == "coder")
        .unwrap_or_else(|| panic!("coder record: {before}"))["id"]
        .clone();
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent n/Ctrl+N]");
    std::fs::write(deck.workdir().join("delegate-now"), "").unwrap();
    common::wait_for_file_trimmed_eq(&deck.workdir().join(SENTINEL), CONTENT, live::BUDGET)
        .unwrap_or_else(|why| {
            panic!(
                "clear=true Codex must act: {why}; CLI={:?}\nGrid:\n{}",
                std::fs::read_to_string(deck.workdir().join("delegate-cli.log")),
                deck.snapshot_grid()
            )
        });
    // The delegate is the orchestration's task message, carried inline or
    // spilled to the worker's task file by its size: either way it must name
    // this task and be acknowledged by a coder other than the one replaced.
    let command = format!(
        "printf '{CONTENT}' > '{}'",
        deck.workdir().join(SENTINEL).display()
    );
    let is_task = |m: &dot_agent_deck::inbox::MessageSummary| {
        let from_orchestration = matches!(
            &m.from,
            dot_agent_deck::inbox::Sender::Agent { orchestration: Some(o), .. } if o == "codex-delegate"
        );
        let carries_task = match m.spill_path.as_deref() {
            Some(path) => path.ends_with(".dot-agent-deck/worker-task-coder.md"),
            None => m
                .text
                .as_deref()
                .is_some_and(|text| text.contains(&command)),
        };
        from_orchestration
            && carries_task
            && m.bind_agent
                .as_deref()
                .is_some_and(|agent| original != agent)
            && m.state == dot_agent_deck::inbox::StateName::Acknowledged
    };
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            inbox_view::all_messages(deck.attach_socket_path())
                .iter()
                .any(is_task)
        }),
        "delegate must be acknowledged through inbox: {:?}",
        inbox_view::all_messages(deck.attach_socket_path())
    );
    let after = inbox::attach(&deck, json!({"op":"list-agents"}));
    assert!(
        after["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["tab_membership"]["role_name"] == "coder" && r["id"] != original),
        "clear=true must replace worker: {after}"
    );
    assert!(
        deck.wait_for_grid_string_within("✉", Duration::from_secs(15)),
        "worker card must show envelope; {}",
        deck.snapshot_grid()
    );
    let log = std::fs::read_to_string(deck.workdir().join("codex-delegate.log")).unwrap();
    assert!(
        log.contains("DoorbellHook"),
        "real UserPromptSubmit must inject task: {log}"
    );
}
