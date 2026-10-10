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

/// Scenario: Launch offline Pi with no credentials and an extension slash
/// command that opens a colour selector. Answer Blue through the deck client;
/// the dialog closes and the extension records and visibly reports Blue.
#[spec("question/live/006")]
#[test]
fn question_live_006_pi_extension_select_without_model() {
    skip_unless!(
        Command::new("pi")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Pi unavailable: {e}"))
            .and_then(|status| if status.success() {
                Ok(())
            } else {
                Err("pi --version failed".into())
            })
    );
    let target = common::harness_tempdir().unwrap();
    let result = target.path().join("question_pi_186c5a9f.json");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pi-question-select.ts");
    let command = format!(
        "pi --offline --no-skills --no-prompt-templates --no-context-files -e {}",
        quote(fixture.to_str().unwrap())
    );
    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("PATH", path())
        .with_env(
            "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS",
            QUESTION_DAEMON_MAX_LIFETIME_SECS,
        )
        .with_env("QUESTION_RESULT_FILE", result.to_string_lossy())
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
