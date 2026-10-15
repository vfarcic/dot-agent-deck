#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! Native inbox delivery as seen in real interactive OpenCode.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/opencode_inbox_live.rs"]
mod live;

use live::{BUDGET, OpenCode};
use spec::spec;

/// Scenario: Start interactive OpenCode, complete a prior prompt, and delegate a sentinel task from a genuine orchestrator CLI. The same shown session must visibly run the framed task and reply, write the sentinel and acknowledge the inbox delivery.
#[spec("inbox/opencode/015")]
#[test]
fn opencode_015_live_idle_delegate_runs_in_shown_session() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 0, true);
    op.prior();
    let session = op.route()["route"]["params"]["sessionID"].clone();
    assert!(session.is_string());
    op.deck.send_bytes(b"\x04");
    op.deck.wait_for_string("[New Agent n/Ctrl+N]");
    let id = op.delegate_body(&format!("Use the shell to run exactly: printf '{}' > '{}'; sleep 5. Then reply DONE-opencode-idle-173ce9.txt and end your turn.",live::CONTENT,op.work.join("opencode-idle-173ce9.txt").display()));
    assert!(
        op.deck.wait_for_grid_string_within("Working", BUDGET),
        "OpenCode's card must visibly work; {}",
        op.evidence()
    );
    op.deck.send_bytes(b"2");
    op.show_message(&id);
    op.done(&id, "opencode-idle-173ce9.txt");
    op.idle();
    assert_eq!(op.route()["route"]["params"]["sessionID"], session);
    assert!(!op.log().contains("doorbell rang"), "{}", op.evidence());
    op.deck.send_bytes(b"\x04");
    assert!(
        op.deck.wait_for_grid_string_within("Idle", BUDGET),
        "completed worker card must be Idle; {}",
        op.evidence()
    );
}

/// Scenario: Complete a prompt, invoke /new, and leave an unsent draft before a real delegate. OpenCode must show the task in a new session, preserve the draft, and leave the old session's messages unchanged.
#[spec("inbox/opencode/016")]
#[test]
fn opencode_016_live_new_session_preserves_draft_and_old_session() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 0, true);
    op.prior();
    let old = op.route()["route"]["params"]["sessionID"].clone();
    let before = op.turns(std::slice::from_ref(&old));
    op.prompt("/new");
    assert!(
        common::wait_until(BUDGET, || op.route()["route"]["name"] == "home"),
        "/new must show home; {}",
        op.evidence()
    );
    let _input = op.input("DRAFT-KEEP-71b3");
    op.wait_screen("DRAFT-KEEP-71b3");
    let id = op.delegate("opencode-new-814ced.txt");
    op.done(&id, "opencode-new-814ced.txt");
    op.wait_screen("DRAFT-KEEP-71b3");
    assert_ne!(op.route()["route"]["params"]["sessionID"], old);
    let after = op.turns(std::slice::from_ref(&old));
    assert_eq!(after, before, "previous session must gain no message");
}

/// Scenario: Seed three older root sessions in a scratch HOME and launch fresh interactive OpenCode. A delegate must create and show a fourth root using the argv test model, with no message added to an older root.
#[spec("inbox/opencode/017")]
#[test]
fn opencode_017_live_fresh_launch_avoids_older_roots_and_uses_argv_model() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 3, true);
    let roots = op.roots();
    assert_eq!(roots.len(), 3);
    let sessions = roots.iter().map(|r| r["id"].clone()).collect::<Vec<_>>();
    let before = op.turns(&sessions);
    let id = op.delegate("opencode-fresh-cc318a.txt");
    op.done(&id, "opencode-fresh-cc318a.txt");
    let shown = op.route()["route"]["params"]["sessionID"].clone();
    assert!(shown.is_string());
    assert!(!roots.iter().any(|r| r["id"] == shown));
    let transcript = op.transcript();
    assert_eq!(
        op.turns(&sessions),
        before,
        "older sessions must gain no messages or changed text"
    );
    let model = common::opencode_test_model().split_once('/').unwrap();
    assert!(
        transcript["message"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["session_id"] == shown
                && m["data"]["role"] == "user"
                && m["data"]["model"]["providerID"] == model.0
                && m["data"]["model"]["modelID"] == model.1),
        "new session must use argv model: {transcript}"
    );
}

/// Scenario: While OpenCode executes a slow real shell tool, delegate an append task. The TUI must show QUEUED and the delegated task must append only after the current tool, then visibly reply and acknowledge the inbox delivery.
#[spec("inbox/opencode/018")]
#[test]
fn opencode_018_live_busy_delegates_show_queued_and_execute_in_order() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 0, true);
    let path = &op.work;
    let mut input=op.input(&format!("Use the shell to run exactly: printf started > '{}/busy-start'; sleep 30; printf '0\\n' > '{}/order.txt'. Then reply BUSY-DONE.",path.display(),path.display()));
    op.wait_screen("busy-start");
    input.write("\r");
    common::wait_for_file_trimmed_eq(&path.join("busy-start"), "started", BUDGET).unwrap();
    let first = op.delegate_body(&format!(
        "Use the shell to run exactly: printf '1\\n' >> '{}/order.txt'. Then reply FIRST-DONE.",
        path.display()
    ));
    op.wait_screen("QUEUED");
    common::wait_for_file_trimmed_eq(&path.join("order.txt"), "0\n1", BUDGET)
        .unwrap_or_else(|why| panic!("queued order: {why}; {}", op.evidence()));
    op.wait_reply(&first);
    op.idle();
    inbox::state(&op.deck, &op.pane, &first, "acknowledged");
}

/// Scenario: Ask real OpenCode to create a general subagent, then show that child's genuine session. A delegate must navigate back to the root, execute there and show the reply and sentinel without adding the task to the child.
#[spec("inbox/opencode/019")]
#[test]
fn opencode_019_live_child_view_navigates_to_root_for_delegate() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 0, true);
    let mut input=op.input("Use the task tool once with subagent_type general and prompt: Reply with only the number equal to 700 plus 31, without tools. After it returns, reply with only the number equal to 900 plus 42.");
    op.wait_screen("subagent_type general");
    input.write("\r");
    op.wait_answered("900 plus 42", "942");
    op.wait_screen("942");
    op.idle();
    let child = op.transcript()["session"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["parent_id"].is_string())
        .cloned()
        .expect("real task tool must create child session");
    let root = child["parent_id"].clone();
    op.navigate(child["id"].as_str().unwrap());
    op.wait_screen("731");
    let id = op.delegate("opencode-child-fc84a1.txt");
    op.done(&id, "opencode-child-fc84a1.txt");
    assert_eq!(op.route()["route"]["params"]["sessionID"], root);
    assert!(
        !op.transcript()["part"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["session_id"] == child["id"]
                && p["data"]["text"].as_str().is_some_and(|t| t.contains(&id))),
        "child must not receive deck task"
    );
}

/// Scenario: Leave a draft in real opencode --mini and delegate a sentinel task. Without further user input, the redraw nudge must show the framed message and final reply, preserve the draft and acknowledge the task.
#[spec("inbox/opencode/020")]
#[test]
fn opencode_020_live_mini_redraws_native_delivery_and_keeps_draft() {
    skip_unless!(live::available());
    let op = OpenCode::new(true, 0, true);
    let _input = op.input("MINI-DRAFT-93ab");
    op.wait_screen("MINI-DRAFT-93ab");
    let id = op.delegate("opencode-mini-ac62f1.txt");
    // The delegate includes role instructions long enough to scroll its opener
    // above mini's viewport. Its closing frame remains beside the visible work.
    op.wait_screen(&format!("[end of dot-agent-deck message {id}]"));
    op.done(&id, "opencode-mini-ac62f1.txt");
    op.wait_screen("MINI-DRAFT-93ab");
    assert_eq!(
        inbox::listing(&op.deck, &op.pane)["component"],
        "opencode_mini"
    );
}

/// Scenario: Kill the captured inbox watcher belonging to this real OpenCode TUI. The plugin must respawn a different watcher after backoff and deliver the next genuine delegate natively with a visible reply and sentinel.
#[spec("inbox/opencode/021")]
#[test]
#[cfg(target_os = "linux")]
fn opencode_021_live_watcher_respawns_and_next_delegate_is_native() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 0, true);
    op.prior();
    let pids = op.watcher_pids();
    assert_eq!(pids.len(), 1, "capture exactly this pane's watcher");
    assert_eq!(unsafe { libc::kill(pids[0], libc::SIGKILL) }, 0);
    assert!(
        common::wait_until(BUDGET, || {
            let current = op.watcher_pids();
            current.len() == 1 && current != pids
        }),
        "watcher must respawn; {}",
        op.evidence()
    );
    inbox::armed(&op.deck, &op.pane, 1);
    let id = op.delegate("opencode-respawn-31c2ef.txt");
    op.done(&id, "opencode-respawn-31c2ef.txt");
    assert!(!op.log().contains("doorbell rang"), "{}", op.evidence());
}

/// Scenario: Remove the deck entry from scratch tui.json so no watcher arms, and leave real OpenCode idle after a prior prompt. The doorbell must be intercepted by the server plugin, run the task without a model-side inbox fetch, and acknowledge its visible reply and sentinel.
#[spec("inbox/opencode/022")]
#[test]
fn opencode_022_live_unarmed_doorbell_is_intercepted_before_model() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 0, false);
    op.prior();
    let id = op.delegate("opencode-doorbell-52a1cd.txt");
    // The unarmed server plugin acknowledges through the CLI, so the inbox
    // records Explicit rather than the armed stream's PluginConfirmed.
    op.done_with_ack(&id, "opencode-doorbell-52a1cd.txt", "explicit");
    assert!(op.log().contains("doorbell rang"), "{}", op.evidence());
    assert!(op.log().contains("DoorbellHook"), "{}", op.evidence());
    assert!(
        !op.transcript()["part"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["data"]["type"] == "tool"
                && p["data"]["state"]["input"]
                    .to_string()
                    .contains("inbox fetch")),
        "model must not run inbox fetch"
    );
    assert!(
        !op.transcript()["part"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["data"]["type"] == "text"
                && p["data"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("waiting, run: dot-agent-deck inbox fetch"))),
        "saved prompt must contain injected block, not doorbell"
    );
}
