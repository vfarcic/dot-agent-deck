#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! Native inbox delivery as seen in real interactive OpenCode.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/opencode_inbox_live.rs"]
mod live;

use dot_agent_deck::event::{AgentType, EventType};
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

/// Scenario: While OpenCode executes a slow real shell tool, delegate an append task. The TUI must show QUEUED (OpenCode 1.x; 2.x draws no such label) and the delegated task must append only after the current tool, then visibly reply and acknowledge the inbox delivery.
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
    // OpenCode 1.x labels a prompt waiting behind a running turn `QUEUED`;
    // 2.x draws no such label (measured on 2.2.5: no "queue" text on screen
    // after a second prompt was submitted mid-turn), so on 2.x the order the
    // work lands in below is what shows the task waited.
    if !common::opencode_test_is_v2() {
        op.wait_screen("QUEUED");
    }
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

/// Scenario: Leave a draft in real opencode --mini and delegate a sentinel task that prints an assembled execution token. Without further input, both views must show the ID-correlated frame and execution, the card must show delivered once, and the draft must survive; after a genuine Idle hook, both views must lose the busy footer within ten seconds.
#[spec("inbox/opencode/020")]
#[test]
fn opencode_020_live_mini_redraws_native_delivery_and_keeps_draft() {
    skip_unless!(live::available());
    let op = OpenCode::new(true, 0, true);
    let _input = op.input("MINI-DRAFT-93ab");
    op.wait_screen("MINI-DRAFT-93ab");
    let id = op.delegate_execution("opencode-mini-ac62f1.txt", "020-93ab");
    // The delegate includes role instructions long enough to scroll its opener
    // above mini's viewport. Its closing frame remains beside the visible work.
    op.wait_screen(&format!("[end of dot-agent-deck message {id}]"));
    op.visible_execution(&id, "opencode-mini-ac62f1.txt", "NATIVE-EXEC-020-93ab");
    op.mini_not_busy_after_idle();
    op.wait_screen("MINI-DRAFT-93ab");
    assert!(
        op.deck
            .wait_for_grid_string_within("MINI-DRAFT-93ab", BUDGET),
        "draft must remain in attached deck; {}",
        op.evidence()
    );
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

/// Scenario: Remove the deck entry from scratch tui.json so no TUI watcher arms, start an explicitly cancellable shell wait with a completion reply marker, and interrupt it once after its running sleep is visible in both views to obtain a genuine Idle and empty composer. The server plugin must intercept the doorbell before the model, visibly execute the delegated task in both views without restarting the wait or emitting its completion reply marker, write the sentinel, acknowledge once, and show a delivered card. This covers delivery after explicit cancellation; the deck does not cancel arbitrary earlier requests.
#[spec("inbox/opencode/022")]
#[test]
fn opencode_022_live_unarmed_doorbell_is_intercepted_before_model() {
    skip_unless!(live::available());
    let op = OpenCode::new(false, 0, false);
    let priming_call = op.interrupted_cancellable_wait();
    let id = op.delegate_execution("opencode-doorbell-52a1cd.txt", "022-52a1");
    // PRD #1666 §4: the server plugin's doorbell-only receiver commits the
    // substituted attempt on its own receipt stream, so the inbox records
    // PluginConfirmed, as for the armed stream.
    op.visible_execution(&id, "opencode-doorbell-52a1cd.txt", "NATIVE-EXEC-022-52a1");
    let transcript = op.transcript();
    let parts = transcript["part"].as_array().unwrap();
    assert!(
        !parts.iter().any(|p| p["data"]["type"] == "tool"
            && p["id"] != priming_call
            && p["data"]["state"]["input"]["command"]
                .as_str()
                .is_some_and(|command| command.contains("sleep 30"))),
        "cancelled priming wait must not run again after delivery; {}",
        op.evidence()
    );
    assert!(
        !parts.iter().any(|p| p["data"]["type"] == "text"
            && p["data"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("PRIME-INTERRUPT-022"))),
        "cancelled wait's completion reply marker must not appear through delegated execution; {}",
        op.evidence()
    );
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
        "interception must deliver before the model can fetch the tested task"
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

/// Scenario: Start a live interactive OpenCode 2.x on the cheap test model in a
/// deck pane and type a prompt that writes a uniquely named sentinel file with
/// the shell. Its card must visibly go Working with the command as tool detail,
/// write the sentinel, and settle Idle still showing the prompt; then a genuine
/// orchestrator delegate must reach the same agent, be acted on (a second
/// sentinel) and read acknowledged.
#[spec("inbox/opencode/060")]
#[test]
fn opencode_060_live_opencode_2_shows_status_and_runs_a_delegate() {
    skip_unless!(common::require_opencode_major(2));
    skip_unless!(common::check_opencode_available());
    let op = OpenCode::new(false, 0, true);
    let events = op.deck.subscribe_events();
    let (agent, pane) = (op.pane.agent.clone(), op.pane.id.clone());
    // This pane's OpenCode, by agent id or, where a hook carried none, pane id.
    let from_agent = |event: &dot_agent_deck::event::AgentEvent| {
        event.agent_type == AgentType::OpenCode
            && (event.agent_id.as_deref() == Some(agent.as_str())
                || event.pane_id.as_deref() == Some(pane.as_str()))
    };

    // A prompt the user types, as they would in the pane. The path is
    // relative to OpenCode's working directory (the project) so the command
    // names the sentinel within the deck's shortened tool detail, which an
    // absolute scratch path pushes past its cut.
    const PROMPTED: &str = "opencode2-prompt-5e81b4.txt";
    let prompt = format!(
        "Use the shell to run exactly: printf '{}' > {PROMPTED}; sleep 3. Then reply DONE and end your turn.",
        live::CONTENT,
    );
    op.prompt(&prompt);
    op.deck.send_bytes(b"\x04");
    op.deck.wait_for_string("[New Agent n/Ctrl+N]");
    assert!(
        op.deck.wait_for_grid_string_within("Working", BUDGET),
        "the OpenCode 2 card must visibly go Working; {}",
        op.evidence()
    );
    events.wait_for(
        |event| {
            from_agent(event)
                && event
                    .user_prompt
                    .as_deref()
                    .is_some_and(|text| text.contains(PROMPTED))
        },
        BUDGET,
    );
    let tool = events.wait_for(
        |event| {
            from_agent(event)
                && event.event_type == EventType::ToolStart
                && event
                    .tool_detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(PROMPTED))
        },
        BUDGET,
    );
    assert!(
        op.deck.wait_for_grid_string_within(PROMPTED, BUDGET),
        "the card must show the command ({:?}) as tool detail; {}",
        tool.tool_name,
        op.evidence()
    );
    common::wait_for_file_trimmed_eq(&op.work.join(PROMPTED), live::CONTENT, BUDGET)
        .unwrap_or_else(|why| {
            panic!(
                "real OpenCode 2 must act on the prompt: {why}; {}",
                op.evidence()
            )
        });
    events.wait_for(
        |event| {
            from_agent(event)
                && event.event_type == EventType::Idle
                && event.timestamp > tool.timestamp
        },
        BUDGET,
    );
    op.idle();
    assert!(
        op.deck
            .wait_for_grid_predicate_within(BUDGET, |grid| grid.contains("Idle")
                && grid.contains(PROMPTED)),
        "the settled card must read Idle and still show the last prompt; {}",
        op.evidence()
    );

    // A genuine `dot-agent-deck delegate` from the orchestrator role.
    const DELEGATED: &str = "opencode2-delegate-9c27d0.txt";
    let id = op.delegate_body(&format!(
        "Use the shell to run exactly: printf '{}' > '{}'. Then reply DONE and end your turn.",
        live::CONTENT,
        op.work.join(DELEGATED).display()
    ));
    common::wait_for_file_trimmed_eq(&op.work.join(DELEGATED), live::CONTENT, BUDGET)
        .unwrap_or_else(|why| {
            panic!(
                "real OpenCode 2 must act on the delegate: {why}; {}",
                op.evidence()
            )
        });
    inbox::state(&op.deck, &op.pane, &id, "acknowledged");
    op.idle();
    assert!(
        op.deck.wait_for_grid_string_within("Idle", BUDGET),
        "the worker card must settle Idle after the delegate; {}",
        op.evidence()
    );
}
