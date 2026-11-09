#![cfg(all(feature = "e2e", unix))]

//! The real hook CLI's version source, with version-only agent stand-ins.

mod common;

use std::cell::RefCell;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dot_agent_deck::event::AgentEvent;
use dot_agent_deck::question::{
    AnswerChannel, PendingQuestion, QuestionAnswer, QuestionReply, ReleaseReason,
};
use serde_json::Value;
use spec::spec;

struct Hook(Child);

impl Drop for Hook {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn question(agent: &str, version: &str, fixture: &str) -> PendingQuestion {
    let temp = common::harness_tempdir().unwrap();
    let program = if agent == "claude-code" {
        "claude"
    } else {
        agent
    };
    let executable = temp.path().join(program);
    // The PATH contains only this version-only stand-in: no real agent runs.
    std::fs::write(&executable, format!(
        "#!/bin/sh\n[ \"$1\" = --version ] || exit 1\n: > \"$HOME/probed\"\nprintf '%s\\n' '{version}'\n"
    )).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let payload = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-questions")
            .join(fixture),
    )
    .unwrap();
    let socket = temp.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut hook = Hook(
        Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["hook", "--agent", agent])
            .env_clear()
            .env("HOME", temp.path())
            .env("PATH", temp.path())
            .env("DOT_AGENT_DECK_SOCKET", &socket)
            .env("DOT_AGENT_DECK_PANE_ID", "version-pane")
            .env("DOT_AGENT_DECK_AGENT_ID", "version-agent")
            .env("DOT_AGENT_DECK_LOG", temp.path().join("deck.log"))
            .env("OTEL_SDK_DISABLED", "true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    hook.0
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let accepted = RefCell::new(None);
    assert!(
        common::wait_until(Duration::from_secs(15), || match listener.accept() {
            Ok((stream, _)) => {
                accepted.replace(Some(stream));
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("accept hook: {e}"),
        }),
        "hook never sent its question"
    );
    let mut stream = accepted.into_inner().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).unwrap();
    let frame: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(frame["message_type"], "question");
    let event: AgentEvent = serde_json::from_value(frame["event"].clone()).unwrap();
    let question = event.question().expect("hook carries a pending question");
    let reply = if question.channel == AnswerChannel::Held {
        assert_eq!(frame["hold"], true);
        let answers = question
            .validate(
                &[QuestionAnswer {
                    question_index: 0,
                    option_indices: vec![1],
                    text: None,
                }],
                false,
            )
            .expect("held allow remains answerable on a newer minor");
        QuestionReply::answered(&question.id, answers)
    } else {
        assert_eq!(frame["hold"], false);
        QuestionReply::released(&question.id, ReleaseReason::NotHeld)
    };
    writeln!(stream, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
    drop(stream);
    {
        let child = RefCell::new(&mut hook.0);
        assert!(
            common::wait_until(deadline.saturating_duration_since(Instant::now()), || {
                child.borrow_mut().try_wait().unwrap().is_some()
            }),
            "hook did not exit after its reply"
        );
    }
    assert!(hook.0.try_wait().unwrap().unwrap().success());
    assert_eq!(
        temp.path().join("probed").exists(),
        fixture != "claude-permission-bash.json"
    );
    if question.channel == AnswerChannel::Held {
        let decision: Value = serde_json::from_reader(hook.0.stdout.take().unwrap()).unwrap();
        assert_eq!(
            decision["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
    }
    question
}

/// Scenario: The real Codex hook finds a version-only binary on its PATH and reports a permission question.
/// A newer minor leaves its options visible but keyboard-only; a patch of the verified minor still accepts voice answers.
#[spec("question/detect/006")]
#[test]
fn question_detect_006_hook_probes_codex_before_offering_keys() {
    for (version, covered) in [
        ("codex-cli 0.160.99", true),
        ("codex-cli 0.161.0", false),
        ("unreadable", true),
    ] {
        let q = question("codex", version, "codex-permission-request.json");
        assert_eq!(
            q.channel,
            if covered {
                AnswerChannel::Keys
            } else {
                AnswerChannel::Unsupported
            }
        );
        assert!(
            q.questions[0]
                .options
                .iter()
                .all(|o| o.keyboard_only != covered)
        );
        let form = question("codex", version, "codex-request-user-input.json");
        assert_eq!(form.channel, q.channel);
    }
}

/// Scenario: Claude's real hook finds a newer minor on PATH before reporting plan approval.
/// The plan is keyboard-only, while the same version's Bash permission still prints a hook allow decision.
#[spec("question/detect/004")]
#[test]
fn question_detect_004_hook_probes_claude_without_disabling_held_answers() {
    for (version, covered) in [
        ("2.1.999 (Claude Code)", true),
        ("2.2.0 (Claude Code)", false),
    ] {
        let plan = question("claude-code", version, "claude-permission-plan.json");
        assert_eq!(
            plan.channel,
            if covered {
                AnswerChannel::Keys
            } else {
                AnswerChannel::Unsupported
            }
        );
        assert_eq!(plan.questions[0].options[0].keyboard_only, !covered);
        let permission = question("claude-code", version, "claude-permission-bash.json");
        assert_eq!(permission.channel, AnswerChannel::Held);
        assert!(!permission.questions[0].options[0].keyboard_only);
        assert!(
            !permission.questions[0]
                .options
                .last()
                .unwrap()
                .keyboard_only
        );
    }
}

/// Scenario: A version-only Devin and a Claude-shaped permission payload pass through the real hook CLI.
/// Its documented allow and deny stay available within 3000.11.x and become keyboard-only on 3000.12.0.
#[spec("question/detect/009")]
#[test]
fn question_detect_009_hook_probes_devin_before_offering_keys() {
    for (version, covered) in [("3000.11.3", true), ("3000.12.0", false)] {
        let q = question("devin", version, "codex-permission-request.json");
        assert_eq!(
            q.channel,
            if covered {
                AnswerChannel::Keys
            } else {
                AnswerChannel::Unsupported
            }
        );
        let answerable: Vec<_> = q.questions[0]
            .options
            .iter()
            .filter(|o| o.answerable())
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(
            answerable,
            if covered {
                vec!["Allow once", "Deny"]
            } else {
                vec![]
            }
        );
    }
}
