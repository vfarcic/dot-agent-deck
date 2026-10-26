//! PRD #1666 part 3, round 5-wake (plan rev 2 §6): what the deck captures as
//! a generation's termination, through the real reader (`pump_reader`) and
//! capture (`capture_termination`): read faults, the wrapper's report of the
//! command it ran against the wrapper's own status, and removal.

use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use spec::spec;

use super::*;
use crate::inbox_display::{PaneTermination, TerminationSource};

/// A reader that answers each `read` with the next scripted step, then EOF.
struct Scripted(std::collections::VecDeque<io::Result<Vec<u8>>>);

impl Read for Scripted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.pop_front() {
            None => Ok(0),
            Some(Err(error)) => Err(error),
            Some(Ok(bytes)) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
        }
    }
}

fn spawn_on(registry: &Arc<AgentPtyRegistry>, pane: &str, command: &str) -> String {
    registry
        .spawn_agent(SpawnOptions {
            command: Some(command),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn")
}

/// What `agent_id`'s reader captured, once it has.
fn captured(registry: &AgentPtyRegistry, agent_id: &str) -> Termination {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(termination) = registry.terminations.lock().unwrap().get(agent_id) {
            return termination.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{agent_id}'s reader captured no termination"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run the real reader over `steps` for a generation the registry holds
/// with its child still running, and return the bytes it published and what
/// it captured.
fn pump(steps: Vec<io::Result<Vec<u8>>>) -> (Vec<u8>, Termination) {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "termination-reader-pane";
    let agent = spawn_on(&registry, pane, "sleep 30");
    let bus = Arc::new(AgentBus::new());
    pump_reader(
        Box::new(Scripted(steps.into())),
        Arc::clone(&bus),
        Arc::new(AtomicBool::new(false)),
        Arc::default(),
        Arc::new(Notify::new()),
        Arc::downgrade(&registry),
        agent.clone(),
        Some(pane.to_string()),
        None,
    );
    let termination = captured(&registry, &agent);
    registry.shutdown_all();
    (bus.snapshot(), termination)
}

/// Scenario: The pane's reader fails with an I/O error while the command the deck started is still running. The capture must say the connection ended, naming the read error, rather than that the command exited.
#[spec("inbox/status/032")]
#[test]
fn status_032_an_injected_read_error_ends_the_connection() {
    let (bytes, termination) = pump(vec![
        Ok(b"before the fault".to_vec()),
        Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        Ok(b"never read".to_vec()),
    ]);
    assert!(String::from_utf8_lossy(&bytes).contains("before the fault"));
    assert!(!String::from_utf8_lossy(&bytes).contains("never read"));
    assert_eq!(
        termination,
        Termination::ConnectionEnded {
            read_end: ReadEnd::Error(io::ErrorKind::BrokenPipe)
        }
    );
    assert_eq!(
        termination.wire(),
        PaneTermination::ConnectionEnded {
            read_end: "broken_pipe".to_string()
        }
    );
}

/// Scenario: The pane's reader is interrupted by a signal and then reads more output before its end. The interruption must be retried, so the later output is published and the capture names the end of the stream, not the interruption.
#[spec("inbox/status/032")]
#[test]
fn status_032_an_interrupted_read_is_retried() {
    let (bytes, termination) = pump(vec![
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Ok(b"after the interrupt".to_vec()),
    ]);
    assert!(
        String::from_utf8_lossy(&bytes).contains("after the interrupt"),
        "the read after the interrupt was published"
    );
    assert_eq!(
        termination,
        Termination::ConnectionEnded {
            read_end: ReadEnd::Eof
        }
    );
}

/// A generation whose child the deck treats as its wrapper and which exits
/// with code 3, with `report` (if any) recorded as the wrapper's account of
/// the command it ran, before the child exits.
fn wrapped_exit(report: Option<&str>) -> Termination {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "termination-wrapper-pane";
    let agent = spawn_on(&registry, pane, "sh -c 'sleep 1; exit 3'");
    registry
        .inner
        .lock()
        .agents
        .get_mut(&agent)
        .expect("registered")
        .child_role = ChildRole::Wrapper;
    // Another pane's and another generation's reports are not this one's.
    registry.note_wrapper_child_exit("another-pane", &agent, "9");
    registry.note_wrapper_child_exit(pane, "another-agent", "9");
    if let Some(report) = report {
        registry.note_wrapper_child_exit(pane, &agent, report);
        // Only the first report counts.
        registry.note_wrapper_child_exit(pane, &agent, "8");
    }
    let termination = captured(&registry, &agent);
    registry.shutdown_all();
    termination
}

/// Scenario: A wrapper reports that the command it ran exited with code 5, while the wrapper itself exits with code 3. The capture must name the child's code 5 as reported by the wrapper, not the wrapper's own status.
#[spec("inbox/status/032")]
#[test]
fn status_032_the_wrappers_report_wins_over_its_own_status() {
    assert_eq!(
        wrapped_exit(Some("5")),
        Termination::PaneCommandExited {
            code: 5,
            role: ChildRole::Wrapper,
            reported_by: TerminationSource::Wrapper,
        }
    );
}

/// Scenario: A wrapper exits with code 3 without reporting how the command it ran exited, or reporting a signal. The capture must fall back to the wrapper's own status, as the deck observed it.
#[spec("inbox/status/032")]
#[test]
fn status_032_without_a_report_the_wrappers_status_is_the_decks() {
    for report in [None, Some("signal:9")] {
        assert_eq!(
            wrapped_exit(report),
            Termination::PaneCommandExited {
                code: 3,
                role: ChildRole::Wrapper,
                reported_by: TerminationSource::Deck,
            },
            "{report:?}"
        );
    }
}

/// Scenario: One generation is respawned in its pane, another is closed, and a third is still running when the deck shuts down. The capture must say the first pane was replaced and the other two were closed.
#[spec("inbox/status/032")]
#[tokio::test]
async fn status_032_removal_is_replaced_or_closed() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let replaced = spawn_on(&registry, "termination-respawn-pane", "sleep 30");
    let successor = registry
        .respawn_agent_for_pane("termination-respawn-pane", "sleep 30")
        .await
        .expect("respawn");
    assert_ne!(successor, replaced);
    let closed = spawn_on(&registry, "termination-close-pane", "sleep 30");
    registry.close_agent(&closed).expect("close");
    let (replaced, closed) = tokio::task::spawn_blocking({
        let registry = Arc::clone(&registry);
        move || (captured(&registry, &replaced), captured(&registry, &closed))
    })
    .await
    .unwrap();
    assert_eq!(replaced, Termination::Replaced);
    assert_eq!(closed, Termination::Closed);
    registry.shutdown_all();
    let shut_down = tokio::task::spawn_blocking({
        let registry = Arc::clone(&registry);
        move || captured(&registry, &successor)
    })
    .await
    .unwrap();
    assert_eq!(shut_down, Termination::Closed, "the shutdown closed it");
}

/// Scenario: A respawn replaces a pane with a command that exits at once. The removed generation's capture must still say it was replaced, however soon the successor ended.
#[spec("inbox/status/032")]
#[tokio::test]
async fn status_032_a_successor_that_exits_at_once_still_replaced_its_predecessor() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "termination-brief-successor-pane";
    let replaced = spawn_on(&registry, pane, "sleep 30");
    let successor = registry
        .respawn_agent_for_pane(pane, "exit 7")
        .await
        .expect("respawn");
    let (replaced, successor) = tokio::task::spawn_blocking({
        let registry = Arc::clone(&registry);
        move || {
            (
                captured(&registry, &replaced),
                captured(&registry, &successor),
            )
        }
    })
    .await
    .unwrap();
    assert_eq!(replaced, Termination::Replaced);
    assert!(
        matches!(successor, Termination::PaneCommandExited { code: 7, .. }),
        "{successor:?}"
    );
    registry.shutdown_all();
}

/// Lift `agent` out of the registry as a respawn does before it terminates
/// the old child: its record counted as respawning, under a real
/// [`RespawnTicket`]. The lifted generation is returned with its child still
/// running, so its own reader stays quiet while the test captures for it.
fn lift_for_respawn<'a>(
    registry: &'a AgentPtyRegistry,
    agent: &str,
) -> (RespawnTicket<'a>, RunningAgent) {
    let mut inner = registry.inner.lock();
    let lifted = inner.agents.remove(agent).expect("registered");
    let record = AgentPtyRegistry::cleanup_record(agent, &lifted);
    lifted.removal.respawning();
    inner.respawning_members.insert(agent.to_string(), record);
    inner.respawns_in_flight += 1;
    drop(inner);
    (
        RespawnTicket {
            registry,
            agent_id: agent.to_string(),
            removal: Arc::clone(&lifted.removal),
        },
        lifted,
    )
}

/// Publish `command` on `pane` as the successor of the respawn holding
/// `ticket`, through the spawn path a respawn takes.
fn publish_successor(
    registry: &Arc<AgentPtyRegistry>,
    ticket: &RespawnTicket<'_>,
    pane: &str,
    command: &str,
) -> String {
    registry
        .spawn_agent_with_dir(
            SpawnOptions {
                command: Some(command),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                ..SpawnOptions::default()
            },
            None,
            Admission::Respawn {
                ticket,
                replacement: None,
            },
        )
        .expect("publish the successor")
}

/// What the removed generation `lifted` captures, while `settle` runs beside
/// the capture on another thread.
fn capture_removed(
    registry: &AgentPtyRegistry,
    agent: &str,
    lifted: &RunningAgent,
    settle: impl FnOnce() + Send,
) -> Termination {
    let removal = Arc::clone(&lifted.removal);
    std::thread::scope(|scope| {
        let capture =
            scope.spawn(|| registry.capture_termination(agent, &removal, false, ReadEnd::Eof));
        settle();
        capture.join().expect("capture")
    })
}

/// What the removed generation `lifted` captures now.
fn capture_now(registry: &AgentPtyRegistry, agent: &str, lifted: &RunningAgent) -> Termination {
    capture_removed(registry, agent, lifted, || {})
}

/// Scenario: A respawn removes a generation and publishes its successor in time, publishes it only after the capture's bound, or ends without one, after or before the removed generation's capture starts. Only the published successor reads as replaced; a replacement still pending at the bound, or one that failed, reads as the pane having ended, in words that say neither replaced nor closed.
#[spec("inbox/status/032")]
#[test]
fn status_032_a_replacement_counts_only_once_its_successor_is_published() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "termination-pending-pane";
    let ended = |termination: &Termination| {
        assert_eq!(termination.wire(), PaneTermination::Unknown);
        assert_eq!(
            serde_json::to_value(termination.wire()).unwrap(),
            serde_json::json!({ "kind": "unknown" })
        );
        assert_eq!(
            termination.wire().words(),
            crate::inbox_display::TERMINATION_UNKNOWN
        );
    };

    // In time: the successor is published, then the respawn returns.
    let first = spawn_on(&registry, pane, "sleep 30");
    let (ticket, mut lifted) = lift_for_respawn(&registry, &first);
    let termination = capture_removed(&registry, &first, &lifted, || {
        std::thread::sleep(Duration::from_millis(50));
        publish_successor(&registry, &ticket, pane, "sleep 30");
    });
    assert_eq!(termination, Termination::Replaced);
    drop(ticket);
    assert_eq!(
        capture_now(&registry, &first, &lifted),
        Termination::Replaced,
        "the respawn returning after its successor changes nothing"
    );
    let _ = lifted.child.kill();
    let successor = registry.pane_current_agent_id(pane).expect("successor");

    // Delayed: nothing is published by the bound, and the replacement then
    // succeeds. The capture had already said only that the pane ended.
    let (ticket, mut lifted) = lift_for_respawn(&registry, &successor);
    let termination = capture_now(&registry, &successor, &lifted);
    assert_eq!(termination, Termination::ReplacementPending);
    ended(&termination);
    publish_successor(&registry, &ticket, pane, "sleep 30");
    drop(ticket);
    let _ = lifted.child.kill();
    let late = registry
        .pane_current_agent_id(pane)
        .expect("late successor");

    // Failed while the capture waits: the respawn returns, publishing nothing.
    let (ticket, mut lifted) = lift_for_respawn(&registry, &late);
    let termination = capture_removed(&registry, &late, &lifted, || {
        std::thread::sleep(Duration::from_millis(50));
        drop(ticket);
    });
    assert_eq!(termination, Termination::ReplacementFailed);
    ended(&termination);
    let _ = lifted.child.kill();

    // Failed before the capture starts: the ticket is gone, and the
    // generation's own evidence still tells the failure from a close, as
    // often as it is read.
    let again = spawn_on(&registry, pane, "sleep 30");
    let (ticket, mut lifted) = lift_for_respawn(&registry, &again);
    drop(ticket);
    for _ in 0..2 {
        assert_eq!(
            capture_now(&registry, &again, &lifted),
            Termination::ReplacementFailed
        );
    }
    let _ = lifted.child.kill();
    registry.shutdown_all();
}

/// Scenario: A respawn fails, and its removed generation's reader is delayed while a hundred later respawns in other panes fail too. When the delayed reader finally captures, it must still say the replacement failed, never that the pane was closed.
#[spec("inbox/status/032")]
#[test]
fn status_032_a_delayed_reader_keeps_its_failed_respawn_however_many_follow() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let delayed = spawn_on(&registry, "termination-delayed-pane", "sleep 30");
    let (ticket, mut delayed_lifted) = lift_for_respawn(&registry, &delayed);
    drop(ticket);
    for n in 0..100 {
        let agent = spawn_on(&registry, &format!("termination-later-{n}"), "true");
        let (ticket, mut lifted) = lift_for_respawn(&registry, &agent);
        drop(ticket);
        assert_eq!(
            capture_now(&registry, &agent, &lifted),
            Termination::ReplacementFailed
        );
        let _ = lifted.child.kill();
    }
    let termination = capture_now(&registry, &delayed, &delayed_lifted);
    assert_eq!(termination, Termination::ReplacementFailed);
    assert_eq!(termination.wire(), PaneTermination::Unknown);
    let _ = delayed_lifted.child.kill();
    registry.shutdown_all();
}

/// Scenario: A respawn publishes a successor that exits straight away, before the respawn returns and before the removed generation's reader captures. The removed generation must still read as replaced, not closed or failed.
#[spec("inbox/status/032")]
#[test]
fn status_032_a_successor_already_exited_at_capture_still_counts() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "termination-exited-successor-pane";
    let first = spawn_on(&registry, pane, "sleep 30");
    let (ticket, mut lifted) = lift_for_respawn(&registry, &first);
    let successor = publish_successor(&registry, &ticket, pane, "exit 7");
    assert!(
        matches!(
            captured(&registry, &successor),
            Termination::PaneCommandExited { code: 7, .. }
        ),
        "the successor exited before the capture"
    );
    drop(ticket);
    assert_eq!(
        capture_now(&registry, &first, &lifted),
        Termination::Replaced
    );
    let _ = lifted.child.kill();
    registry.shutdown_all();
}

/// Scenario: A reader ends for a generation the registry does not hold and that nothing recorded removing. The capture must say only that the pane ended, never that it was closed or replaced.
#[spec("inbox/status/032")]
#[test]
fn status_032_a_removal_nothing_recorded_is_unknown() {
    let registry = AgentPtyRegistry::new();
    let termination = registry.capture_termination(
        "termination-unrecorded",
        &RemovalEvidence::default(),
        false,
        ReadEnd::Eof,
    );
    assert_eq!(termination, Termination::Unknown);
    assert_eq!(termination.wire(), PaneTermination::Unknown);
}
