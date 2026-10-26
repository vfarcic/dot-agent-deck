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

/// Scenario: One generation is respawned in its pane and another is closed. The capture must say the first pane was replaced and the second was closed.
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
}
