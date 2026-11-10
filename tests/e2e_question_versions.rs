#![cfg(all(feature = "e2e", unix))]

//! Real hook CLI version discovery, with version-only agent stand-ins.
dot_agent_deck_test_harness::link!();

use std::cell::RefCell;
use std::io::{BufRead as _, Write as _};
use std::os::unix::{fs::PermissionsExt as _, net::UnixListener};
use std::process::{Command, Stdio};
use std::time::Duration;

use dot_agent_deck::question::{
    AnswerChannel, PendingQuestion, QuestionKind, QuestionReply, ReleaseReason,
};
use serde_json::Value;
use spec::spec;

fn install_version(root: &std::path::Path, program: &str, output: &str) {
    let executable = root.join(program);
    let probe_log = root.join("probe.log");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] || exit 1\nprintf 'probe\\n' >> '{}'\nprintf '%s\\n' '{}'\n",
            probe_log.display(), output
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn hook_question(root: &std::path::Path, agent: &str, fixture: &str) -> PendingQuestion {
    let socket = root.join("hook.sock");
    let _ = std::fs::remove_file(&socket);
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
        .env("PATH", root)
        .env("HOME", root)
        .env("DOT_AGENT_DECK_STATE_DIR", root.join("state"))
        .env("DOT_AGENT_DECK_SOCKET", socket)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", root.join("attach.sock"))
        .env("DOT_AGENT_DECK_LOG", root.join("deck.log"))
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
    server.join().unwrap()
}

fn expire_version_cache(root: &std::path::Path) {
    let cache = std::fs::read_dir(root.join("state/question-versions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut entry: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    entry["checked_at"] = serde_json::to_value(std::time::SystemTime::UNIX_EPOCH).unwrap();
    std::fs::write(cache, serde_json::to_vec(&entry).unwrap()).unwrap();
}

/// Scenario: Run the real hook CLI with Codex and Claude version-only stand-ins and captured question payloads.
/// Newer minors make typed answers keyboard-only, while a verified patch and an unknown version remain answerable; repeated hook processes reuse the version until the executable changes, and held Claude answers do not probe.
#[spec("question/detect/021")]
#[test]
fn question_detect_021_hook_probes_versions_before_offering_typed_answers() {
    for (version, covered) in [
        ("codex-cli 0.160.99", true),
        ("codex-cli 0.161.0", false),
        ("unknown", true),
    ] {
        let root = common::harness_tempdir().unwrap();
        install_version(root.path(), "codex", version);
        for phase in ["cold", "cached"] {
            let q = hook_question(root.path(), "codex", "codex-permission-request.json");
            assert_eq!(
                q.channel,
                if covered {
                    AnswerChannel::Keys
                } else {
                    AnswerChannel::Unsupported
                },
                "{version}, {phase}"
            );
            assert_eq!(q.has_answerable_option(), covered, "{version}");
            assert!(
                q.questions[0]
                    .options
                    .iter()
                    .all(|o| o.keyboard_only != covered)
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("probe.log")).unwrap(),
            "probe\n",
            "unchanged {version} was probed again"
        );
        let (replacement, replacement_channel) = if covered {
            ("codex-cli 0.161.99", AnswerChannel::Unsupported)
        } else {
            ("codex-cli 0.160.0", AnswerChannel::Keys)
        };
        install_version(root.path(), "codex", replacement);
        let q = hook_question(root.path(), "codex", "codex-permission-request.json");
        assert_eq!(q.channel, replacement_channel);
        assert_eq!(
            std::fs::read_to_string(root.path().join("probe.log")).unwrap(),
            "probe\nprobe\n",
            "a changed executable was not probed again"
        );
    }
    let root = common::harness_tempdir().unwrap();
    install_version(root.path(), "codex", "codex-cli 0.161.0");
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.path().join("codex"))
        .unwrap()
        .write_all(b"exit 1\n")
        .unwrap();
    for phase in ["cold", "cached"] {
        let q = hook_question(root.path(), "codex", "codex-permission-request.json");
        assert_eq!(
            q.channel,
            AnswerChannel::Keys,
            "a {phase} failed probe is unknown"
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.log")).unwrap(),
        "probe\n",
        "a failed probe must not delay each prompt"
    );
    expire_version_cache(root.path());
    let q = hook_question(root.path(), "codex", "codex-permission-request.json");
    assert_eq!(q.channel, AnswerChannel::Keys);
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.log")).unwrap(),
        "probe\nprobe\n",
        "a transient failure must be retried after expiry"
    );
    let root = common::harness_tempdir().unwrap();
    install_version(root.path(), "codex-real", "codex-cli 0.160.99");
    let launcher = root.path().join("codex");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nexec '{}' \"$@\"\n",
            root.path().join("codex-real").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let q = hook_question(root.path(), "codex", "codex-permission-request.json");
    assert_eq!(q.channel, AnswerChannel::Keys);
    install_version(root.path(), "codex-real", "codex-cli 0.161.99");
    // The launcher's identity did not move with its target's upgrade. Even a
    // successful cached output must expire rather than bless that target forever.
    expire_version_cache(root.path());
    let q = hook_question(root.path(), "codex", "codex-permission-request.json");
    assert_eq!(
        q.channel,
        AnswerChannel::Unsupported,
        "an unchanged launcher kept its old version after cache expiry"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.log")).unwrap(),
        "probe\nprobe\n"
    );
    let root = common::harness_tempdir().unwrap();
    install_version(root.path(), "claude", "2.2.0 (Claude Code)");
    let q = hook_question(root.path(), "claude-code", "claude-permission-plan.json");
    assert_eq!(q.channel, AnswerChannel::Unsupported);
    assert!(q.questions[0].options.iter().all(|o| o.keyboard_only));
    assert_eq!(
        std::fs::read_to_string(root.path().join("probe.log")).unwrap(),
        "probe\n"
    );
    let root = common::harness_tempdir().unwrap();
    install_version(root.path(), "claude", "2.2.0 (Claude Code)");
    for fixture in [
        "claude-permission-bash.json",
        "claude-permission-write.json",
        "claude-ask-user-question-form.json",
    ] {
        let q = hook_question(root.path(), "claude-code", fixture);
        assert_eq!(q.channel, AnswerChannel::Held);
        assert!(!q.questions[0].options[0].keyboard_only);
        assert_eq!(
            q.questions[0].options.last().unwrap().keyboard_only,
            q.kind == QuestionKind::Choice,
            "the form's Chat about this option stays keyboard-only"
        );
        assert!(q.has_answerable_option());
        assert!(
            !root.path().join("probe.log").exists(),
            "{fixture}: held answers must not run --version"
        );
    }
}
