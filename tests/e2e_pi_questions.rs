#![cfg(all(feature = "e2e", unix))]

//! Credential-free Pi extension dialog: no prompt and no model request.

mod common;
#[path = "common/questions.rs"]
mod questions;

use common::TuiDeck;
use dot_agent_deck::daemon_client::AnswerReport;
use dot_agent_deck::question::{AnswerChannel, QuestionKind};
use questions::*;
use spec::spec;
use std::process::{Command, Stdio};

fn pi_available() -> Result<(), String> {
    Command::new("pi")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Pi unavailable: {e}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err("pi --version failed".into())
            }
        })
}

/// Offline Pi with the colour-selector extension, writing its answer to
/// `result`, in a deck that also gets `env`; its selector is open on return.
fn pi_deck_with_selector(result: &std::path::Path, env: &[(&str, String)]) -> (TuiDeck, String) {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pi-question-select.ts");
    let command = format!(
        "pi --offline --no-skills --no-prompt-templates --no-context-files -e {}",
        quote(fixture.to_str().unwrap())
    );
    let mut builder = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("PATH", path())
        .with_env(
            "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS",
            QUESTION_DAEMON_MAX_LIFETIME_SECS,
        )
        .with_env("QUESTION_RESULT_FILE", result.to_string_lossy());
    for (key, value) in env {
        builder = builder.with_env(*key, value.clone());
    }
    let deck = builder
        .without_agent_credentials()
        .with_continue_session("question-pi", command)
        .launch_with_fixture("minimal");
    let id = agent_id(&deck);
    visible(&deck, "ctrl+c");
    deck.send_keys(b"/question-select");
    visible(&deck, "/question-select");
    assert!(
        deck.send_keys_until_grid_string_within(b"\r", "question_pi_186c5a9f colour", TURN),
        "Pi slash command did not open dialog:\n{}",
        deck.snapshot_grid()
    );
    (deck, id)
}

/// Scenario: Launch offline Pi with no credentials and an extension slash
/// command that opens a colour selector. Answer Blue through the deck client;
/// the dialog closes and the extension records and visibly reports Blue.
#[spec("question/live/006")]
#[test]
fn question_live_006_pi_extension_select_without_model() {
    skip_unless!(pi_available());
    let target = common::harness_tempdir().unwrap();
    let result = target.path().join("question_pi_186c5a9f.json");
    let (deck, id) = pi_deck_with_selector(&result, &[]);
    let q = pending(&deck, &id, QuestionKind::Choice);
    assert_eq!(q.channel, AnswerChannel::Held);
    assert_eq!(q.questions.len(), 1);
    assert_eq!(
        q.questions[0]
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>(),
        ["Red", "Green", "Blue"]
    );
    assert!(!result.exists(), "dialog resolved before answer");
    assert_eq!(
        answer(&deck, &id, &q, vec![selection(0, &[3])]),
        AnswerReport::Answered
    );
    assert!(
        common::wait_until(TURN, || std::fs::read_to_string(&result)
            .is_ok_and(|s| s == "\"Blue\"")),
        "Pi extension did not receive Blue:\n{}",
        deck.snapshot_grid()
    );
    cleared(&deck, &id);
    visible(&deck, "Selected Blue");
    assert!(
        !deck.snapshot_grid().contains("question_pi_186c5a9f colour"),
        "Pi dialog stayed open"
    );
}

/// Scenario: Launch offline Pi with the question notification set to one
/// minute, open the colour selector and leave it unanswered. The card shows
/// `QUESTION` and the question's age; after a minute the TUI sends exactly one
/// terminal notification naming the question, and no second one after that;
/// and the question, still held, is answered Blue through the deck client
/// (issue #2075).
#[spec("question/live/007")]
#[test]
fn question_live_007_an_unanswered_pi_question_shows_its_age_and_notifies_once() {
    skip_unless!(pi_available());
    let target = common::harness_tempdir().unwrap();
    let result = target.path().join("question_pi_186c5a9f.json");
    let config = target.path().join("config.toml");
    std::fs::write(&config, "[notifications]\nquestion_after_minutes = 1\n").unwrap();
    let (deck, id) = pi_deck_with_selector(
        &result,
        &[(
            "DOT_AGENT_DECK_CONFIG",
            config.to_string_lossy().into_owned(),
        )],
    );
    let q = pending(&deck, &id, QuestionKind::Choice);
    assert_eq!(q.channel, AnswerChannel::Held);
    deck.send_keys(b"\x04");
    visible(&deck, "QUESTION ");
    let notification = "\x1b]9;Question waiting 1m";
    assert!(
        !deck.stream_text().contains("\x1b]9;"),
        "a notification before the question had waited a minute"
    );
    assert!(
        deck.wait_for_stream_string_within(notification, std::time::Duration::from_secs(90)),
        "no notification after a minute unanswered:\n{}",
        deck.snapshot_grid()
    );
    visible(&deck, "QUESTION 1m");
    // Still exactly one five seconds on: the TUI checks every frame, so a
    // second notification would land well inside the hold.
    deck.wait_until_grid_then_hold(
        "the question has notified exactly once",
        std::time::Duration::from_secs(5),
        |_| deck.stream_text().matches("\x1b]9;").count() == 1,
    );
    let stream = deck.stream_text();
    assert_eq!(
        stream.matches("\x1b]9;").count(),
        1,
        "the question notified more than once"
    );
    assert!(
        stream.contains("question_pi_186c5a9f colour"),
        "the notification does not name the question"
    );
    assert_eq!(
        answer(&deck, &id, &q, vec![selection(0, &[3])]),
        AnswerReport::Answered
    );
    assert!(
        common::wait_until(TURN, || std::fs::read_to_string(&result)
            .is_ok_and(|s| s == "\"Blue\"")),
        "Pi extension did not receive Blue after the wait:\n{}",
        deck.snapshot_grid()
    );
    cleared(&deck, &id);
}
