#![cfg(all(feature = "e2e", unix))]

//! Real hook CLI version discovery, with version-only agent stand-ins.
mod common;

use std::cell::RefCell;
use std::io::{BufRead as _, Write as _};
use std::os::unix::{fs::PermissionsExt as _, net::UnixListener};
use std::process::{Command, Stdio};
use std::time::Duration;

use dot_agent_deck::question::{AnswerChannel, PendingQuestion, QuestionReply, ReleaseReason};
use serde_json::Value;
use spec::spec;

fn hook_question(agent: &str, program: &str, output: &str, fixture: &str) -> PendingQuestion {
    let root = common::harness_tempdir().unwrap();
    let executable = root.path().join(program);
    let probe_log = root.path().join("probe.log");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] || exit 1\nprintf 'probe\\n' >> '{}'\nprintf '%s\\n' '{}'\n",
            probe_log.display(), output
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = root.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let accepted = RefCell::new(None);
        assert!(
            common::wait_until(Duration::from_secs(30), || {
                *accepted.borrow_mut() = listener.accept().ok();
                accepted.borrow().is_some()
            }),
            "hook did not connect"
        );
        let (stream, _) = accepted.into_inner().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["message_type"], "question");
        let question: PendingQuestion = serde_json::from_str(
            frame["event"]["metadata"][dot_agent_deck::event::QUESTION_METADATA_KEY]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let mut stream = reader.into_inner();
        if frame["hold"] == true {
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&QuestionReply::held(&question.id)).unwrap()
            )
            .unwrap();
        }
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&QuestionReply::released(
                &question.id,
                ReleaseReason::Cleared
            ))
            .unwrap()
        )
        .unwrap();
        question
    });
    let input = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-questions")
            .join(fixture),
    )
    .unwrap();
    let mut child = Command::new(common::contained_deck_binary())
        .args(["hook", "--agent", agent])
        .env_clear()
        .env("PATH", root.path())
        .env("HOME", root.path())
        .env("DOT_AGENT_DECK_SOCKET", socket)
        .env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            root.path().join("attach.sock"),
        )
        .env("DOT_AGENT_DECK_LOG", root.path().join("deck.log"))
        .env("DOT_AGENT_DECK_PANE_ID", "pane-versions")
        .env("DOT_AGENT_DECK_AGENT_ID", "agent-versions")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let child = RefCell::new(child);
    if !common::wait_until(Duration::from_secs(30), || {
        child.borrow_mut().try_wait().unwrap().is_some()
    }) {
        let _ = child.borrow_mut().kill();
        let _ = child.borrow_mut().wait();
        panic!("hook did not finish");
    }
    let result = child.into_inner().wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let question = server.join().unwrap();
    assert_eq!(std::fs::read_to_string(probe_log).unwrap(), "probe\n");
    question
}

/// Scenario: Run the real hook CLI with Codex and Claude version-only stand-ins and captured question payloads.
/// Newer minors make typed answers keyboard-only, while a verified patch and an unknown version remain answerable; Claude's hook permission answers stay answerable on 2.2.
#[spec("question/detect/021")]
#[test]
fn question_detect_021_hook_probes_versions_before_offering_typed_answers() {
    for (version, covered) in [
        ("codex-cli 0.160.99", true),
        ("codex-cli 0.161.0", false),
        ("unknown", true),
    ] {
        let q = hook_question("codex", "codex", version, "codex-permission-request.json");
        assert_eq!(
            q.channel,
            if covered {
                AnswerChannel::Keys
            } else {
                AnswerChannel::Unsupported
            },
            "{version}"
        );
        assert_eq!(q.has_answerable_option(), covered, "{version}");
        assert!(
            q.questions[0]
                .options
                .iter()
                .all(|o| o.keyboard_only != covered)
        );
    }
    let q = hook_question(
        "claude-code",
        "claude",
        "2.2.0 (Claude Code)",
        "claude-permission-plan.json",
    );
    assert_eq!(q.channel, AnswerChannel::Unsupported);
    assert!(q.questions[0].options.iter().all(|o| o.keyboard_only));
    let q = hook_question(
        "claude-code",
        "claude",
        "2.2.0 (Claude Code)",
        "claude-permission-bash.json",
    );
    assert_eq!(q.channel, AnswerChannel::Held);
    assert!(!q.questions[0].options[0].keyboard_only);
    assert!(!q.questions[0].options.last().unwrap().keyboard_only);
    assert!(q.has_answerable_option());
}
