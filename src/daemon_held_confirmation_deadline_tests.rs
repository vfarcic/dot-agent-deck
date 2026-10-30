//! Issue #1976: the `held` confirmation line a held question's producer asked
//! for (`QuestionSignal::confirm_hold`) is written under the connection's idle
//! timeout, so a producer that never reads its connection cannot park the
//! handler before its hold deadline starts and pin a hold-budget permit.

use super::*;
use crate::agent_pty::{DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use crate::event::{AgentEvent, AgentType, EventType};
use crate::question::{QuestionReply, ReleaseReason};
use spec::spec;
use tokio::io::AsyncBufReadExt;

const PANE: &str = "pane-held-confirmation";
const QUESTION: &str = "q-held-confirmation";

/// A Bash permission question raised by `agent_id` on [`PANE`].
fn question_event(agent_id: &str) -> AgentEvent {
    let mut event = AgentEvent {
        session_id: "held-confirmation-session".to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type: EventType::PermissionRequest,
        tool_name: Some("Bash".to_string()),
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata: std::collections::HashMap::new(),
        pane_id: Some(PANE.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    };
    let question = crate::question::claude_permission_request(
        QUESTION.to_string(),
        "Bash",
        None,
        Some("touch x".into()),
        None,
        None,
        1,
        None,
    )
    .question;
    event.set_question(&question);
    event
}

/// An attested agent on [`PANE`] asking for a held question with a `held`
/// confirmation, handled on a connection whose write side buffers
/// `write_capacity` bytes and is never read, and whose read side stays open
/// and silent — a producer that is still connected. Returns the handler's
/// outcome, or `None` when it did not finish within ten seconds, and what
/// the test reads back afterwards.
async fn hold_with_confirmation(
    write_capacity: usize,
) -> (Option<Option<crate::agent_pty::HeldReply>>, Fixture) {
    let registry = Arc::new(AgentPtyRegistry::new());
    let agent = registry
        .spawn_agent(SpawnOptions {
            command: Some("/bin/sh"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn shell agent");
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
    state.write().await.register_pane(PANE.to_string());
    let (event_tx, _rx) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
    let signal = crate::event::QuestionSignal {
        pane_id: PANE.to_string(),
        token: None,
        event: question_event(&agent),
        hold: true,
        ack: false,
        confirm_hold: true,
    };
    // The producer's ends: it keeps both open, sends nothing more, and reads
    // `producer_reads` only when the test does.
    let (producer_writes, daemon_reads) = tokio::io::duplex(64);
    let (daemon_writes, producer_reads) = tokio::io::duplex(write_capacity);
    let mut reader = tokio::io::BufReader::new(daemon_reads);
    let mut writer = daemon_writes;
    let holds = Arc::new(tokio::sync::Semaphore::new(1));
    let mut slot = Some(
        Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap(),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        handle_question_signal(
            signal,
            Some(agent.clone()),
            false,
            &state,
            &event_tx,
            &registry,
            QuestionConnection {
                reader: &mut reader,
                writer: &mut writer,
                slot: &mut slot,
                holds: &holds,
                hold_deadline: Duration::from_millis(500),
                write_timeout: Duration::from_millis(300),
            },
        ),
    )
    .await
    .ok();
    (
        outcome,
        Fixture {
            registry,
            state,
            holds,
            _producer_writes: producer_writes,
            producer_reads: tokio::io::BufReader::new(producer_reads),
        },
    )
}

struct Fixture {
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    holds: Arc<tokio::sync::Semaphore>,
    _producer_writes: tokio::io::DuplexStream,
    producer_reads: tokio::io::BufReader<tokio::io::DuplexStream>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.registry.shutdown_all();
    }
}

/// Scenario: An attested agent's producer asks for a held question with a `held` confirmation, on a connection it keeps open but never reads, whose send buffer has no room for that line. The daemon must give up on the line within the connection's idle timeout and release the hold, as it does for a producer that hangs up: the handler finishes, the hold-budget permit comes back, and the pane no longer holds or shows the question. A control producer that reads its connection is told `held` and released at the hold deadline.
#[spec("question/hold/024")]
#[tokio::test]
async fn question_hold_024_a_producer_that_never_reads_cannot_pin_a_hold() {
    // Control: a producer that reads gets its `held` line, and the hold
    // deadline then releases it, so a stall below is the write's alone.
    let (outcome, mut reading) = hold_with_confirmation(64 * 1024).await;
    let held = outcome
        .expect("a reading producer's hold ends at the hold deadline")
        .expect("a reading producer is answered");
    assert_eq!(
        held.reply,
        QuestionReply::released(QUESTION, ReleaseReason::Cleared)
    );
    let mut line = String::new();
    reading.producer_reads.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<QuestionReply>(line.trim()).unwrap(),
        QuestionReply::held(QUESTION)
    );
    assert_eq!(reading.holds.available_permits(), 1);
    drop(reading);

    // A producer whose send buffer has no room for the `held` line. The
    // handler gives up at the 300 ms write timeout; the bound below leaves
    // room for a loaded box and still refuses a release seconds late.
    let started = tokio::time::Instant::now();
    let (outcome, stalled) = hold_with_confirmation(1).await;
    let elapsed = started.elapsed();
    let outcome = outcome.expect(
        "the `held` write never gave up: a producer that does not read parked the handler \
         before its hold deadline started",
    );
    assert!(
        outcome.is_none(),
        "a producer that cannot be written to is released as one that hung up"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the stalled producer was released after {elapsed:?}, not near the 300 ms write timeout"
    );
    assert_eq!(
        stalled.holds.available_permits(),
        1,
        "the hold-budget permit comes back"
    );
    assert!(!stalled.registry.question_holds().is_held(PANE, QUESTION));
    assert!(
        stalled
            .state
            .read()
            .await
            .pending_question_id_on_pane(PANE)
            .is_none(),
        "the pane no longer shows the question"
    );
}
