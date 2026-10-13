#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! Native inbox delivery through the bundled extension in real interactive Pi.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/pi_inbox_live.rs"]
mod live;

use live::{BUDGET, Pi};
use spec::spec;

/// Scenario: Leave real Pi idle in a PTY-attached deck, then delegate a sentinel task through a genuine orchestrator CLI. Pi must visibly act, label its card with the envelope subject, write the sentinel, and acknowledge the message it commits.
#[spec("inbox/pi/001")]
#[test]
fn pi_001_live_idle_delegate_wakes_and_acknowledges() {
    skip_unless!(live::available());
    let pi = Pi::new();
    pi.idle();
    let id = pi.delegate("pi-idle-36de27.txt");
    pi.done(&id, "pi-idle-36de27.txt");
    assert!(
        pi.deck
            .wait_for_grid_string_within("✉ task from the orchestrator for coder", BUDGET),
        "Pi card must label the native task; {}",
        pi.evidence()
    );
    assert!(
        pi.deck
            .wait_for_stream_string_within("pi-idle-36de27.txt", BUDGET),
        "real Pi work must be visible in its attached pane; {}",
        pi.evidence()
    );
    pi.idle();
    assert!(
        !pi.log().contains("doorbell rang"),
        "idle Pi must receive natively; {}",
        pi.evidence()
    );
}

/// Scenario: While real Pi runs a slow bash tool, post three inbox messages through the client attach API in quick succession. They must append A, B and C after the current tool, enter Pi once each in separate turns, and each become acknowledged.
#[spec("inbox/pi/002")]
#[test]
fn pi_002_live_streaming_messages_run_in_order_one_per_turn() {
    skip_unless!(live::available());
    let pi = Pi::new();
    pi.prompt(&format!("Use bash to run exactly: printf started > '{}/busy-start'; sleep 30; printf '0\\n' > '{}/order.txt'. Then reply BUSY-DONE.",pi.work.display(),pi.work.display()));
    common::wait_for_file_trimmed_eq(&pi.work.join("busy-start"), "started", BUDGET).unwrap();
    let ids=['A','B','C'].map(|letter|pi.post_body(&format!("queued task {letter}"), &format!(
        "Use bash to run exactly: printf '{letter}\\n' >> '{}/order.txt'. Reply DONE-{letter} and end your turn.",pi.work.display()
    )));
    assert!(
        !pi.work.join("order.txt").exists(),
        "three sends must finish while the original tool is streaming"
    );
    common::wait_for_file_trimmed_eq(&pi.work.join("order.txt"), "0\nA\nB\nC", BUDGET)
        .unwrap_or_else(|why| panic!("queued tasks ran out of order: {why}; {}", pi.evidence()));
    for id in &ids {
        inbox::state(&pi.deck, &pi.pane, id, "acknowledged");
        pi.committed_once(id);
    }
    let events = pi.events();
    let positions = ids
        .iter()
        .map(|id| {
            events
                .iter()
                .position(|e| {
                    e["type"] == "message_start"
                        && e["event"]["message"]["role"] == "user"
                        && e["event"]
                            .to_string()
                            .contains(&format!("[dot-agent-deck message {id} "))
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "committed inbox messages must retain send order: {positions:?}"
    );
    for pair in positions.windows(2) {
        assert!(
            events[pair[0] + 1..pair[1]]
                .iter()
                .any(|e| e["type"] == "message_start"
                    && e["event"]["message"]["role"] == "assistant"),
            "a real assistant turn must separate each committed user message: {:?}",
            &events[pair[0]..=pair[1]]
        );
    }
    pi.idle();
}

/// Scenario: Finish a native delegate in real Pi, invoke /new, and inspect only children of its captured process id. Exactly one replacement watcher must remain, and the next delegate must execute and be acknowledged in the new session.
#[spec("inbox/pi/003")]
#[test]
#[cfg(target_os = "linux")]
fn pi_003_live_new_session_keeps_one_watcher_and_receives_next_message() {
    skip_unless!(live::available());
    let pi = Pi::new();
    let id = pi.delegate("pi-before-new-75ab19.txt");
    pi.done(&id, "pi-before-new-75ab19.txt");
    pi.idle();
    let old_session = pi.session();
    let old_watchers = pi.watcher_pids();
    assert_eq!(old_watchers.len(), 1);
    pi.prompt("/new");
    assert!(
        common::wait_until(BUDGET, || pi.session() != old_session
            && pi.watcher_pids().len() == 1
            && pi.watcher_pids() != old_watchers),
        "/new must leave one replacement watcher; {}",
        pi.evidence()
    );
    assert!(
        common::wait_until(BUDGET, || old_watchers
            .iter()
            .all(|pid| !common::process_running(*pid))),
        "old watcher must exit after /new"
    );
    inbox::armed(&pi.deck, &pi.pane, 1);
    let new_session = pi.session();
    let id = pi.delegate("pi-after-new-18c2ba.txt");
    pi.done(&id, "pi-after-new-18c2ba.txt");
    assert!(
        pi.users()
            .iter()
            .any(|e| e["session"] == new_session && e["event"].to_string().contains(&id)),
        "next message must be committed in the new session"
    );
    assert_eq!(pi.watcher_pids().len(), 1);
}

/// Scenario: Start real interactive Pi and capture its process id plus the one inbox watcher directly parented by it. SIGKILL only that Pi process; its watcher must exit on stdin EOF.
#[spec("inbox/pi/004")]
#[test]
#[cfg(target_os = "linux")]
fn pi_004_live_sigkill_exits_the_child_watcher_on_stdin_eof() {
    skip_unless!(live::available());
    let pi = Pi::new();
    let watchers = pi.watcher_pids();
    assert_eq!(watchers.len(), 1);
    assert_eq!(unsafe { libc::kill(pi.pid(), libc::SIGKILL) }, 0);
    assert!(
        common::wait_until(BUDGET, || !common::process_running(watchers[0])),
        "Pi's child watcher must exit on stdin EOF; captured watcher={watchers:?}"
    );
}
