//! Helpers used only by the question PTY tests (not the shared harness).
#![allow(dead_code)]

use super::common::{self, TuiDeck};
use dot_agent_deck::daemon_client::{AnswerReport, DaemonClient};
use dot_agent_deck::question::{PendingQuestion, QuestionAnswer, QuestionKind};
use dot_agent_deck::state::{SessionSnapshot, SessionStatus};
use std::time::Duration;

pub const TURN: Duration = Duration::from_secs(120);

pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn path() -> String {
    format!(
        "{}:{}",
        std::path::Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .parent()
            .unwrap()
            .display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

pub fn agent_id(deck: &TuiDeck) -> String {
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            common::agent_records_on(deck.attach_socket_path()).len() == 1
        }),
        "expected one live pane:\n{}",
        deck.snapshot_grid()
    );
    common::agent_records_on(deck.attach_socket_path())[0]
        .id
        .clone()
}

pub fn snapshot(deck: &TuiDeck, id: &str) -> Option<SessionSnapshot> {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| record.id == id)
        .and_then(|record| record.live)
}

pub fn pending(deck: &TuiDeck, id: &str, kind: QuestionKind) -> PendingQuestion {
    assert!(
        common::wait_until(TURN, || {
            snapshot(deck, id)
                .is_some_and(|live| live.pending_question.is_some_and(|q| q.kind == kind))
        }),
        "no pending {kind:?} question within {TURN:?}; live={:?}\nGrid:\n{}",
        snapshot(deck, id),
        deck.snapshot_grid()
    );
    let live = snapshot(deck, id).expect("live snapshot");
    assert_eq!(live.status, SessionStatus::WaitingForInput);
    live.pending_question.expect("pending question")
}

pub fn selection(question_index: u32, option_indices: &[u32]) -> QuestionAnswer {
    QuestionAnswer {
        question_index,
        option_indices: option_indices.to_vec(),
        text: None,
    }
}

pub fn answer(
    deck: &TuiDeck,
    id: &str,
    q: &PendingQuestion,
    answers: Vec<QuestionAnswer>,
) -> AnswerReport {
    let client = DaemonClient::new(deck.attach_socket_path().to_path_buf());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(30),
                client.answer_question_while(|| true, id, &q.id, answers, false),
            )
            .await
        })
        .expect("answer-question timed out after 30s")
        .expect("answer-question transport")
}

pub fn finished(deck: &TuiDeck, id: &str) {
    assert!(
        common::wait_until(TURN, || {
            snapshot(deck, id).is_some_and(|live| {
                live.pending_question.is_none() && live.status == SessionStatus::Idle
            })
        }),
        "agent did not finish; live={:?}\nGrid:\n{}",
        snapshot(deck, id),
        deck.snapshot_grid()
    );
}

pub fn cleared(deck: &TuiDeck, id: &str) {
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            snapshot(deck, id).is_some_and(|live| live.pending_question.is_none())
        }),
        "pending question did not clear; live={:?}\nGrid:\n{}",
        snapshot(deck, id),
        deck.snapshot_grid()
    );
}

pub fn visible(deck: &TuiDeck, needle: &str) {
    assert!(
        deck.wait_for_grid_string_within(needle, TURN),
        "pane never displayed {needle:?}:\n{}",
        deck.snapshot_grid()
    );
}

pub fn submit(deck: &TuiDeck, prompt: &str, composer: &str) {
    deck.send_keys(prompt.as_bytes());
    // Wait for the complete prompt to paint before Enter, avoiding paste-burst
    // submission races without introducing a timing-only readiness gate.
    let tail = prompt.split_whitespace().last().expect("nonempty prompt");
    visible(deck, tail);
    // A returned composer or the newly reported user prompt proves submission.
    // A fast OpenCode question replaces the composer before it can be observed;
    // status alone can still describe the preceding turn or a mode change.
    let id = agent_id(deck);
    let prefix: String = prompt.chars().take(64).collect();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut submitted = false;
    while std::time::Instant::now() < deadline {
        deck.send_keys(b"\r");
        if deck.wait_for_grid_predicate_within(Duration::from_secs(2), |grid| {
            grid.contains(composer)
                || snapshot(deck, &id).is_some_and(|live| {
                    live.last_user_prompt
                        .is_some_and(|text| text.starts_with(&prefix))
                })
        }) {
            submitted = true;
            break;
        }
    }
    assert!(
        submitted,
        "prompt was not submitted:\n{}",
        deck.snapshot_grid()
    );
}

pub fn assert_form(q: &PendingQuestion, multi: bool, appended: &str) {
    assert_eq!(q.questions.len(), 2, "expected the whole form: {q:?}");
    assert_eq!(q.questions[1].multi_select, multi);
    for question in &q.questions {
        assert!(
            question.options.iter().any(|o| o.label == appended),
            "missing appended option: {q:?}"
        );
    }
    assert_eq!(q.questions[0].options[1].label, "Green");
    assert_eq!(q.questions[1].options[0].label, "Small");
    assert_eq!(q.questions[1].options[2].label, "Large");
}

pub const FORM_PROMPT: &str = "Call the question tool exactly once with two questions. First: question 'Which colour?', header 'Colour', options Red, Green, Blue (each with a short description), single select. Second: question 'Which sizes?', header 'Sizes', options Small, Medium, Large (each with a short description). After the tool returns, flatten all selected labels (including each selected size) in question order, join them by vertical bars without spaces, and report only that result, then stop. Do not answer the questions yourself.";
