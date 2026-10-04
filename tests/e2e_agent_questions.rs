#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Real interactive agents answering permission prompts and whole forms.
//! Cost: short turns on cheap models; requires the developer's credentials.

mod common;
#[path = "common/questions.rs"]
mod questions;

use common::TuiDeck;
use dot_agent_deck::daemon_client::AnswerReport;
use dot_agent_deck::question::{AnswerChannel, AnswerRefusal, OptionRole, QuestionKind};
use questions::*;
use spec::spec;

fn claude() -> TuiDeck {
    TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("PATH", path())
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_continue_session("question-haiku", "claude --model claude-haiku-4-5-20251001 --permission-mode default --tools Bash,AskUserQuestion")
        .launch_with_fixture("minimal")
}

fn submit_claude(deck: &TuiDeck, id: &str, prompt: &str, probe: &str) {
    visible(deck, "Claude Code v");
    let events = deck.subscribe_events();
    deck.submit_claude_prompt(&events, id, prompt, probe);
}

fn assert_claude_permission(deck: &TuiDeck, q: &dot_agent_deck::question::PendingQuestion) {
    assert_eq!(q.channel, AnswerChannel::Held);
    let options = &q.questions[0].options;
    assert_eq!(options[0].label, "Yes");
    assert_eq!(options[0].role, OptionRole::AllowOnce);
    assert_eq!(options.last().unwrap().label, "No");
    assert_eq!(options.last().unwrap().role, OptionRole::Deny);
    visible(deck, "1. Yes");
    visible(deck, &format!("{}. No", options.last().unwrap().index));
    if let Some(always) = options.iter().find(|o| o.role == OptionRole::AllowAlways) {
        assert!(
            always.scope.is_some(),
            "always-allow must name its scope: {q:?}"
        );
        visible(deck, &always.label);
    }
}

/// Scenario: Ask interactive Haiku to touch a uniquely named file outside its
/// working folder, observe Needs Input and the permission menu, and allow once
/// through the client. The file appears and the live agent finishes its turn.
#[spec("question/live/001")]
#[test]
fn question_live_001_claude_permission_allow_once_continues() {
    skip_unless!(common::check_claude_available());
    let target = common::harness_tempdir().unwrap();
    let sentinel = target.path().join("question_touch_71c9a24e.txt");
    let deck = claude();
    let id = agent_id(&deck);
    let prompt = format!(
        "Use Bash to run exactly touch {}. This is the question_touch_71c9a24e check. Do not use another tool or alter the command. After it succeeds, say done and stop.",
        quote(sentinel.to_str().unwrap())
    );
    submit_claude(&deck, &id, &prompt, "question_touch_71c9a24e");
    let q = pending(&deck, &id, QuestionKind::Permission);
    assert_claude_permission(&deck, &q);
    deck.send_keys(b"\x04");
    visible(&deck, "Needs Input");
    deck.send_keys(b"\r");
    visible(&deck, "1. Yes");
    assert!(!sentinel.exists(), "command ran before approval");
    assert_eq!(
        answer(&deck, &id, &q, vec![selection(0, &[1])]),
        AnswerReport::Answered
    );
    assert!(
        common::wait_until(TURN, || sentinel.is_file()),
        "approved command never created sentinel:\n{}",
        deck.snapshot_grid()
    );
    finished(&deck, &id);
    deck.send_keys(b"\x04");
    visible(&deck, "Idle");
}

/// Scenario: Interactive Haiku asks a two-question form with multiple sizes.
/// Answer Green plus Small and Large in one client request; the pane reports
/// all three chosen labels and the pending form clears.
#[spec("question/live/002")]
#[test]
fn question_live_002_claude_whole_multiselect_form() {
    skip_unless!(common::check_claude_available());
    let deck = claude();
    let id = agent_id(&deck);
    let prompt = format!(
        "question_form_248bf106: Use AskUserQuestion. {FORM_PROMPT} The second question must set multiSelect true."
    );
    submit_claude(&deck, &id, &prompt, "question_form_248bf106");
    let q = pending(&deck, &id, QuestionKind::Choice);
    assert_eq!(q.channel, AnswerChannel::Held);
    assert_form(&q, true, "Type something.");
    for question in &q.questions {
        assert!(
            question
                .options
                .iter()
                .any(|o| o.label == "Chat about this" && o.keyboard_only)
        );
    }
    visible(&deck, "Type something.");
    visible(&deck, "Chat about this");
    assert_eq!(
        answer(
            &deck,
            &id,
            &q,
            vec![selection(0, &[2]), selection(1, &[1, 3])]
        ),
        AnswerReport::Answered
    );
    finished(&deck, &id);
    visible(&deck, "Green|Small|Large");
}

/// Scenario: Deny a genuine held Haiku permission prompt with keyboard 3.
/// The snapshot clears the prompt, a late client answer is refused, and the
/// denied command never creates its sentinel file.
#[spec("question/live/003")]
#[test]
fn question_live_003_claude_keyboard_deny_refuses_late_answer() {
    skip_unless!(common::check_claude_available());
    let target = common::harness_tempdir().unwrap();
    let sentinel = target.path().join("question_deny_bf1377d2.txt");
    let deck = claude();
    let id = agent_id(&deck);
    let prompt = format!(
        "question_deny_bf1377d2: Use Bash exactly once to run touch {}. If denied, stop immediately without retrying.",
        quote(sentinel.to_str().unwrap())
    );
    submit_claude(&deck, &id, &prompt, "question_deny_bf1377d2");
    let q = pending(&deck, &id, QuestionKind::Permission);
    assert_claude_permission(&deck, &q);
    assert_eq!(q.questions[0].options.last().unwrap().index, 3);
    deck.send_keys(b"3");
    cleared(&deck, &id);
    assert_eq!(
        answer(&deck, &id, &q, vec![selection(0, &[1])]),
        AnswerReport::Refused(AnswerRefusal::NoPendingQuestion)
    );
    assert!(!sentinel.exists(), "keyboard-denied command ran");
    visible(&deck, "Interrupted");
}

/// Scenario: Codex on a cheap model asks to run a command outside its read-only sandbox;
/// approve through the client and observe the sentinel. Switch to Plan mode,
/// answer a two-question form through keys, and see both answers reported.
#[spec("question/live/004")]
#[test]
fn question_live_004_codex_permission_and_whole_form_by_keys() {
    skip_unless!(if common::codex_test_model().contains("mini")
        || common::codex_test_model() == "gpt-5.6-luna"
    {
        Ok(())
    } else {
        Err(
            "question/live/004 requires a cheap mini or gpt-5.6-luna model in DOT_AGENT_DECK_CODEX_TEST_MODEL"
                .to_string(),
        )
    });
    skip_unless!(common::check_codex_available());
    let target = common::harness_tempdir().unwrap();
    let sentinel = target.path().join("question_codex_93de612b.txt");
    let command = format!(
        "codex --model {} --sandbox read-only --ask-for-approval on-request -c 'model_reasoning_effort=\"low\"'",
        quote(common::codex_test_model())
    );
    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("PATH", path())
        .with_imported_codex_credentials()
        .with_continue_session("question-codex", command)
        .launch_with_fixture("minimal");
    let id = agent_id(&deck);
    visible(&deck, "Ask Codex to do anything");
    submit(
        &deck,
        &format!(
            "Run exactly touch {} using the shell with sandbox_permissions require_escalated and a justification asking approval. Do not use apply_patch or retry without approval. After success say done and stop.",
            quote(sentinel.to_str().unwrap())
        ),
        "Ask Codex to do anything",
    );
    let q = pending(&deck, &id, QuestionKind::Permission);
    assert_eq!(q.channel, AnswerChannel::Keys);
    assert_eq!(
        q.questions[0]
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Yes, proceed",
            "Yes, and don't ask again for commands that start with the prefix shown",
            "No, and tell Codex what to do differently"
        ]
    );
    visible(&deck, "Yes, proceed");
    visible(&deck, "No, and tell Codex what to do differently");
    assert!(!sentinel.exists());
    assert_eq!(
        answer(&deck, &id, &q, vec![selection(0, &[1])]),
        AnswerReport::Answered
    );
    assert!(
        common::wait_until(TURN, || sentinel.is_file()),
        "Codex command did not run:\n{}",
        deck.snapshot_grid()
    );
    finished(&deck, &id);
    visible(&deck, "Ask Codex to do anything");
    let mode_deadline = std::time::Instant::now() + TURN;
    let mut plan_mode = false;
    while std::time::Instant::now() < mode_deadline {
        // The slash-menu description also says "Plan mode". Require the
        // actual footer, and retry if Codex has not finished its turn yet.
        if deck.snapshot_grid().contains("Ask Codex to do anything") {
            deck.send_keys(b"/plan");
        }
        deck.send_keys(b"\r");
        if deck.wait_for_grid_predicate_within(std::time::Duration::from_secs(2), |grid| {
            grid.lines().any(|line| {
                line.contains(deck.workdir().to_string_lossy().as_ref())
                    && line.contains("Plan mode")
            })
        }) {
            plan_mode = true;
            break;
        }
    }
    assert!(
        plan_mode,
        "Codex did not enter Plan mode:\n{}",
        deck.snapshot_grid()
    );
    submit(
        &deck,
        &format!("Use request_user_input. {FORM_PROMPT} Both questions are single select."),
        "Ask Codex to do anything",
    );
    let q = pending(&deck, &id, QuestionKind::Choice);
    assert_eq!(q.channel, AnswerChannel::Keys);
    assert_form(&q, false, "None of the above");
    visible(&deck, "None of the above");
    assert_eq!(
        answer(&deck, &id, &q, vec![selection(0, &[2]), selection(1, &[3])]),
        AnswerReport::Answered
    );
    finished(&deck, &id);
    visible(&deck, "Green|Large");
}

/// Scenario: OpenCode on a cheap model asks permission for Bash and then asks a form
/// with one multiselect question. Answer each through the plugin reply path;
/// the sentinel appears, the form closes, and the chosen labels are reported.
#[spec("question/live/005")]
#[test]
fn question_live_005_opencode_permission_and_multiselect_reply() {
    skip_unless!(if common::opencode_test_model().contains("mini")
        || common::opencode_test_model() == "openai/gpt-5.6-luna"
        || common::opencode_test_model() == common::OPENCODE_TEST_MODEL_DEFAULT
    {
        Ok(())
    } else {
        Err("question/live/005 requires the default Haiku model or a cheap mini or openai/gpt-5.6-luna override in DOT_AGENT_DECK_OPENCODE_TEST_MODEL".to_string())
    });
    skip_unless!(common::check_opencode_available());
    let config = common::harness_tempdir().unwrap();
    let settings = config.path().join("opencode.json");
    std::fs::write(
        &settings,
        r#"{"$schema":"https://opencode.ai/config.json","permission":{"bash":"ask"}}"#,
    )
    .unwrap();
    let command = format!("opencode --model {}", quote(common::opencode_test_model()));
    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .with_env("PATH", path())
        .with_env("OPENCODE_CONFIG", settings.to_string_lossy())
        .with_imported_opencode_credentials()
        .with_continue_session("question-opencode", command)
        .launch_with_fixture("minimal");
    // Bash is configured to ask. Keep its target inside the project so an
    // unrelated external-directory permission cannot become the first question.
    let sentinel = deck.workdir().join("question_opencode_542c01e8.txt");
    let id = agent_id(&deck);
    visible(&deck, "Ask anything");
    submit(
        &deck,
        &format!(
            "Use bash exactly once to run only touch {}. Do not list files, check paths, add commands, or use another tool. After success stop and report done.",
            quote(sentinel.to_str().unwrap())
        ),
        "Ask anything",
    );
    let q = pending(&deck, &id, QuestionKind::Permission);
    assert_eq!(q.channel, AnswerChannel::Held);
    assert_eq!(
        q.questions[0]
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>(),
        ["Allow once", "Allow always", "Reject"]
    );
    visible(&deck, "Allow once");
    visible(&deck, "Allow always");
    assert!(!sentinel.exists());
    assert_eq!(
        answer(&deck, &id, &q, vec![selection(0, &[1])]),
        AnswerReport::Answered
    );
    assert!(
        common::wait_until(TURN, || sentinel.is_file()),
        "OpenCode command did not run; approved={q:?}; live={:?}\n{}",
        snapshot(&deck, &id),
        deck.snapshot_grid()
    );
    finished(&deck, &id);
    submit(
        &deck,
        &format!("Use question. {FORM_PROMPT} The second question must set multiple true."),
        "Ask anything",
    );
    let q = pending(&deck, &id, QuestionKind::Choice);
    assert_eq!(q.channel, AnswerChannel::Held);
    assert_form(&q, true, "Type your own answer");
    assert_eq!(
        answer(
            &deck,
            &id,
            &q,
            vec![selection(0, &[2]), selection(1, &[1, 3])]
        ),
        AnswerReport::Answered
    );
    finished(&deck, &id);
    visible(&deck, "Green|Small|Large");
}
