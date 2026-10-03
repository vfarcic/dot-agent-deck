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
//! Lane 1 (`cargo test-e2e`): every role is a shell stand-in, no agent and no
//! credential.

mod common;

use std::cell::RefCell;
use std::process::Output;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::TabMembership;
use spec::spec;

const STEADY: &str = "steady";
const FRESH: &str = "fresh";

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
        if role_record(deck, STEADY).is_none() || role_record(deck, FRESH).is_none() {
            return false;
        }
        *orchestrator.borrow_mut() = record.pane_id_env;
        true
    });
    assert!(
        ready,
        "the orchestration's three role panes were not registered within 20s; records = {:?}",
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

/// Whether `role`'s CURRENT live agent has the daemon's task pointer for that
/// role in its PTY.
fn role_has_pointer(deck: &TuiDeck, role: &str) -> bool {
    let Some(record) = role_record(deck, role) else {
        return false;
    };
    let snapshot = common::pane_snapshot_on(deck.attach_socket_path(), &record.id);
    squeeze(&String::from_utf8_lossy(&snapshot)).contains(&format!("worker-task-{role}.md"))
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

/// Scenario: Launch the real TUI and its lazy daemon on the `delegate-exited-worker` fixture and open its orchestration. Delegate to the `clear = false` worker `steady` and see the task pointer land, then let that worker report `work-done` and EXIT ON ITS OWN, and delegate to `steady` again: the real `delegate` CLI must exit non-zero naming `steady` as a role that reached no worker. Then let the `clear = true` worker `fresh` exit the same way and delegate to it: the CLI must exit 0 and a fresh `fresh` worker must come back holding the task pointer.
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
        .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0")
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
        "control — the first task pointer never reached the live `steady` worker"
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
    assert!(
        common::wait_until(Duration::from_secs(30), || role_has_pointer(&deck, FRESH)),
        "the respawned `fresh` worker never received its task pointer; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
}
