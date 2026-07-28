#![cfg(feature = "e2e")]

//! PTY-attached coverage for the daemon idle-worker detector. The real binary
//! opens the `orch-deck` fixture's live `cat` role panes, receives a synthetic
//! Delegate over its hook socket, and must render the daemon's idle prompt in
//! the orchestrator surface.

mod common;

use std::cell::RefCell;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::TabMembership;
use dot_agent_deck::event::{DaemonMessage, DelegateSignal};
use spec::spec;

fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> new-pane form
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // select [Orch: demo-orch]
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // submit (Command is hidden)
}

fn orchestration_panes(deck: &TuiDeck) -> (String, String) {
    let panes = RefCell::new(None);
    let ready = common::wait_until(Duration::from_secs(10), || {
        let records = common::agent_records_on(deck.attach_socket_path());
        let orchestrator = records
            .iter()
            .find_map(|record| match &record.tab_membership {
                Some(TabMembership::Orchestration {
                    role_name,
                    is_start_role: true,
                    ..
                }) if role_name == "orchestrator" => record.pane_id_env.clone(),
                _ => None,
            });
        let worker = records
            .iter()
            .find_map(|record| match &record.tab_membership {
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == "worker" => {
                    record.pane_id_env.clone()
                }
                _ => None,
            });
        if let (Some(orchestrator), Some(worker)) = (orchestrator, worker) {
            *panes.borrow_mut() = Some((orchestrator, worker));
            return true;
        }
        false
    });
    assert!(
        ready,
        "orchestration role panes were not registered within 10s; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
    panes
        .into_inner()
        .expect("ready role-pane poll stores both pane ids")
}

/// Scenario: Launch the real TUI and its lazy daemon with a tiny worker-response timeout, open the two-role `orch-deck` fixture, and inject a Delegate from the orchestrator pane to the live `cat` worker over the hook socket. The worker never sends work-done, so the rendered orchestration surface must visibly contain "has not responded" after the timeout.
#[spec("scheduler/idle-worker/011")]
#[test]
fn idle_worker_011_silent_worker_prompt_is_visible_in_attached_tui() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "1500")
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");
    open_orchestration(&deck);
    deck.wait_for_string("worker");

    let (orchestrator_pane, _worker_pane) = orchestration_panes(&deck);
    let message = DaemonMessage::Delegate(DelegateSignal {
        pane_id: orchestrator_pane,
        task: "Remain silent so the idle detector can surface its prompt.".to_string(),
        to: vec!["worker".to_string()],
        timestamp: chrono::Utc::now(),
    });
    let line = serde_json::to_string(&message).expect("serialize Delegate hook message");
    common::write_hook_line(deck.hook_socket_path(), &line)
        .expect("inject Delegate over hook socket");

    deck.wait_for_string("has not responded");
}
