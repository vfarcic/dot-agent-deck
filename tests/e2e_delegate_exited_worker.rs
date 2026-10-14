#![cfg(all(feature = "e2e", unix))]

//! PTY-attached coverage for a `delegate` aimed at a worker whose process has
//! EXITED ON ITS OWN — crashed, quit, or ran a command that ended — rather than
//! being closed (issue #524).
//!
//! Nothing on the natural-exit path takes a role pane out of the daemon's
//! routing maps, so after a natural exit the role still resolves to that pane. What the
//! orchestrator is then told is the whole question, and it is answered by the
//! real `dot-agent-deck delegate` binary's exit code and message: a `clear =
//! false` role has no live worker left to receive the task, so the delegate
//! must fail and say which role missed, while a `clear = true` role is
//! respawned by the delegate itself and must still be delivered.
//!
//! Lane 1 (`cargo test-e2e`): every role is a stand-in (`cat`, or the
//! fixture's `worker.py`), no agent and no credential.

mod common;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[cfg(all(feature = "e2e", unix))]
#[path = "support/inbox_route.rs"]
mod route;
#[path = "support/inbox_view.rs"]
mod view;

use std::cell::RefCell;
use std::process::Output;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::TabMembership;
use spec::spec;

const STEADY: &str = "steady";
const FRESH: &str = "fresh";
const QUITTER: &str = "quitter";

/// Drop every whitespace run, so a needle that straddles a wrap column in a
/// PTY snapshot still matches.
fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The production new-pane flow: `Ctrl+n` → confirm dir → Right selects the
/// `[Orch: demo-orch]` chip → Enter → Enter. It is what registers the
/// daemon-side role maps `delegate` routes on.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
}

/// The live registry record of `role`, if it has one. The daemon's `ListAgents`
/// leaves exited agents out, so `None` is how an exited worker reads.
fn role_record(deck: &TuiDeck, role: &str) -> Option<dot_agent_deck::agent_pty::AgentRecord> {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| {
            matches!(
                &record.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
            )
        })
}

/// The orchestrator's pane id, once every role of the orchestration is live.
fn wait_for_roles(deck: &TuiDeck) -> String {
    let orchestrator = RefCell::new(None);
    let ready = common::wait_until(Duration::from_secs(20), || {
        let Some(record) = role_record(deck, "orchestrator") else {
            return false;
        };
        if [STEADY, FRESH, QUITTER]
            .iter()
            .any(|role| role_record(deck, role).is_none())
        {
            return false;
        }
        *orchestrator.borrow_mut() = record.pane_id_env;
        true
    });
    assert!(
        ready,
        "the orchestration's four role panes were not registered within 20s; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
    orchestrator
        .into_inner()
        .expect("the orchestrator has a pane id")
}

/// Run the REAL `delegate` CLI as the orchestrator's pane, the way its shell
/// would, and hand back what it printed and how it exited.
fn delegate(deck: &TuiDeck, orchestrator_pane: &str, role: &str, task: &str) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["delegate", "--to", role, "--task", task])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", orchestrator_pane)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run the real `dot-agent-deck delegate` CLI")
}

fn describe(output: &Output) -> String {
    format!(
        "exit={:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Whether the agent that holds `role`'s pane now has been posted a task —
/// PRD #1666 S5: what a delegate does instead of typing a pointer that the
/// fixture's `worker.py` read from its PTY. `_name` is the fixture worker the
/// test expects to hold the pane; the binding to the live agent is what checks
/// it.
fn worker_read_pointer(deck: &TuiDeck, _name: &str, role: &str) -> bool {
    let Some(record) = role_record(deck, role) else {
        return false;
    };
    let Some(pane) = record.pane_id_env.as_deref() else {
        return false;
    };
    view::messages(deck.attach_socket_path(), pane)
        .into_iter()
        .any(|message| {
            message.bind_agent.as_deref() == Some(record.id.as_str())
                && message.subject.starts_with("task from the orchestrator")
        })
}

/// [`worker_read_pointer`] for a role's own fixture worker.
fn role_has_pointer(deck: &TuiDeck, role: &str) -> bool {
    worker_read_pointer(deck, role, role)
}

/// Resize `role`'s live pane by one row, through the daemon's attach socket —
/// what the TUI does to every role pane when it lays out a freshly opened
/// orchestration tab. A resize drops the pane's replay ring, so whatever the
/// worker printed before it is gone from the pane snapshot (issue #1539).
fn resize_role_pane(deck: &TuiDeck, role: &str) {
    let record = role_record(deck, role).unwrap_or_else(|| panic!("{role} must be live"));
    let resized = common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::Resize {
            id: record.id.clone(),
            rows: record.rows.saturating_sub(1).max(1),
            cols: record.cols,
            viewer: None,
        },
    );
    assert!(
        resized.as_ref().is_ok_and(|response| response.ok),
        "resize {role}'s pane: {resized:?}"
    );
}

/// Let `role`'s worker report its work and then exit on its own, and wait until
/// the daemon no longer lists it as a live agent.
fn let_worker_exit(deck: &TuiDeck, role: &str) {
    let gone_agent = role_record(deck, role)
        .unwrap_or_else(|| panic!("{role} must be live before it is told to exit"))
        .id;
    std::fs::write(deck.workdir().join(format!("{role}-exit")), b"go\n")
        .unwrap_or_else(|e| panic!("create {role}'s exit trigger: {e}"));
    let exited = common::wait_until(Duration::from_secs(20), || {
        role_record(deck, role).is_none_or(|record| record.id != gone_agent)
    });
    assert!(
        exited,
        "{role}'s worker never exited on its own; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
}

/// Start a different agent, running `command`, in the pane of `exited` — a
/// worker that has exited on its own — through the daemon's real `StartAgent`,
/// the request a TUI sends when it opens a pane. Returns the new agent's id.
fn start_successor(
    deck: &TuiDeck,
    exited: &dot_agent_deck::agent_pty::AgentRecord,
    command: String,
) -> String {
    use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};

    let pane = exited
        .pane_id_env
        .clone()
        .expect("the exited worker's pane id");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime for the attach client");
    let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
    runtime
        .block_on(client.start_agent(StartAgentOptions {
            command: Some(command),
            cwd: exited.cwd.clone(),
            display_name: exited.display_name.clone(),
            env: vec![
                (
                    dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID.to_string(),
                    pane,
                ),
                (
                    "DAD_TEST_BIN".to_string(),
                    env!("CARGO_BIN_EXE_dot-agent-deck").to_string(),
                ),
            ],
            tab_membership: exited.tab_membership.clone(),
            ..StartAgentOptions::default()
        }))
        .expect("start a successor on the exited worker's pane id")
}

/// Scenario: Launch the real TUI and its lazy daemon on the `delegate-exited-worker` fixture and open its orchestration. Delegate to the `clear = false` worker `steady` and see the task reach its inbox, then let that worker report `work-done` and EXIT ON ITS OWN, and delegate to `steady` again: the real `delegate` CLI must exit non-zero naming `steady` as a role that reached no worker. Then let the `clear = true` worker `fresh` exit the same way and delegate to it: the CLI must exit 0 and a fresh `fresh` worker must come back with the task in its inbox.
#[spec("orchestration/delegate/049")]
#[test]
fn delegate_049_a_delegate_to_a_worker_that_exited_on_its_own_is_not_reported_delivered() {
    let deck = TuiDeck::builder()
        // The delegate is run from the test process as the orchestrator's pane,
        // with no hook token: the provenance gate is not what this is about.
        .impersonating_pane_signals()
        .with_pty_size(160, 40)
        // Both delegation watches off: neither detector is under test, and a
        // notice firing into the orchestrator would only add noise.
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DAD_TEST_BIN", env!("CARGO_BIN_EXE_dot-agent-deck"))
        .launch_with_fixture("delegate-exited-worker");
    deck.wait_for_string("No active agents");
    open_orchestration(&deck);
    let orchestrator_pane = wait_for_roles(&deck);

    // ---- CONTROL: the role is reachable while its worker lives ------------
    let first = delegate(&deck, &orchestrator_pane, STEADY, "Do the first task.");
    assert!(
        first.status.success(),
        "control — a delegate to a LIVE `steady` worker must succeed, or nothing below is about \
         an exited one\n{}",
        describe(&first)
    );
    assert!(
        common::wait_until(Duration::from_secs(30), || role_has_pointer(&deck, STEADY)),
        "control — the first task never reached the live `steady` worker"
    );
    // Issue #1539: the TUI may still be laying out the new tab when the pointer
    // lands, and its resize drops the pane's replay ring. Doing that resize
    // here, after delivery, keeps the evidence honest about it every run.
    resize_role_pane(&deck, STEADY);
    assert!(
        role_has_pointer(&deck, STEADY),
        "issue #1539: a resize after delivery hid the first task"
    );

    // ---- THE NATURAL EXIT -------------------------------------------------
    // The worker reports (so it owes nothing, and the next delegate cannot be
    // refused as busy) and then its process ends. Nothing closes the pane.
    let_worker_exit(&deck, STEADY);

    // ---- A DELEGATE TO THE EXITED `clear = false` WORKER ------------------
    let second = delegate(&deck, &orchestrator_pane, STEADY, "Do the second task.");
    let stderr = String::from_utf8_lossy(&second.stderr).into_owned();
    assert!(
        !second.status.success(),
        "issue #524: `steady` is `clear = false` and its worker has exited, so nothing can \
         receive this task — yet `delegate` reported it delivered\n{}",
        describe(&second)
    );
    assert!(
        squeeze(&stderr).contains(&squeeze(&format!(
            "reached no worker for role(s): {STEADY}"
        ))),
        "the failure must name the role that missed\n{}",
        describe(&second)
    );

    // ---- `clear = true`: the delegate respawns the role, so it lands ------
    let_worker_exit(&deck, FRESH);
    let third = delegate(&deck, &orchestrator_pane, FRESH, "Do the fresh task.");
    assert!(
        third.status.success(),
        "a `clear = true` role whose worker exited is respawned by the delegate, so this must \
         still be delivered — refusing it would turn a working delivery into a failure\n{}",
        describe(&third)
    );
    // PRD #1666 S5: the task is posted to the replacement as soon as the
    // respawn produced it; nothing waits for a `SessionStart`.
    assert!(
        common::wait_until(Duration::from_secs(90), || role_has_pointer(&deck, FRESH)),
        "the respawned `fresh` worker never received its task; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
}

/// Scenario: Complete a commissioned task, delegate again, then let the worker exit naturally and start a successor in the same pane. Its report reaches the orchestrator inbox as uncommissioned work and leaves the earlier commissioned report file intact.
#[spec("orchestration/work-done/015")]
#[test]
fn work_done_015_a_successor_in_an_exited_worker_s_pane_is_not_credited_with_its_task() {
    route::successor_worker_report();
}

/// Scenario: Launch the real TUI on the `delegate-exited-worker` fixture and delegate to the `clear = false` worker `quitter`; a second delegate to it is refused as busy while it lives and owes that task. Let it EXIT ON ITS OWN without reporting and start a different agent in the same pane through the daemon's real `StartAgent`. The real `delegate` CLI must then exit 0 without `--supersede`, and the task must be posted to the successor.
#[spec("orchestration/delegate/051")]
#[test]
fn delegate_051_a_live_successor_in_an_exited_worker_s_pane_is_not_refused_as_busy() {
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(160, 40)
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DAD_TEST_BIN", env!("CARGO_BIN_EXE_dot-agent-deck"))
        .launch_with_fixture("delegate-exited-worker");
    deck.wait_for_string("No active agents");
    open_orchestration(&deck);
    let orchestrator_pane = wait_for_roles(&deck);
    let quitter = role_record(&deck, QUITTER).expect("the quitter is live");

    // ---- A DELEGATION OWED BY THE FIRST WORKER ----------------------------
    let delegated = delegate(&deck, &orchestrator_pane, QUITTER, "Do the quitter's task.");
    assert!(
        delegated.status.success(),
        "control — a delegate to the live, idle `quitter` must succeed\n{}",
        describe(&delegated)
    );
    assert!(
        common::wait_until(Duration::from_secs(30), || role_has_pointer(&deck, QUITTER)),
        "control — the task never reached the live `quitter` worker"
    );

    // ---- CONTROL: the busy refusal is live while that worker owes it -------
    let refused = delegate(&deck, &orchestrator_pane, QUITTER, "Do another task.");
    assert!(
        !refused.status.success()
            && squeeze(&String::from_utf8_lossy(&refused.stderr))
                .contains(&squeeze("still owes a work-done")),
        "control — a second delegate to the LIVE `quitter`, which still owes a work-done, must \
         be refused as busy (#580), or the assertion below proves nothing\n{}",
        describe(&refused)
    );

    // ---- THE NATURAL EXIT, NO REPORT ---------------------------------------
    let_worker_exit(&deck, QUITTER);

    // ---- A SUCCESSOR IN THE SAME PANE, never delegated to -------------------
    let successor = start_successor(
        &deck,
        &quitter,
        "python3 -u worker.py successor".to_string(),
    );
    assert_ne!(successor, quitter.id, "the successor is a different agent");
    assert!(
        common::wait_until(Duration::from_secs(20), || {
            role_record(&deck, QUITTER).is_some_and(|record| record.id == successor)
        }),
        "the successor never became `quitter`'s live agent; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );

    // ---- A DELEGATE TO THE ROLE, WITHOUT --supersede ------------------------
    let reply = delegate(
        &deck,
        &orchestrator_pane,
        QUITTER,
        "Do the successor's task.",
    );
    assert!(
        reply.status.success(),
        "issue #1531: the successor owes nothing, yet the delegate was refused because of the \
         exited worker's outstanding delegation\n{}",
        describe(&reply)
    );
    assert!(
        common::wait_until(Duration::from_secs(30), || worker_read_pointer(
            &deck,
            "successor",
            QUITTER
        )),
        "the successor never received the task; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
}
