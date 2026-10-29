#![cfg(feature = "e2e")]
//! SC-08 clap and no-daemon refusals, entirely inside per-command sandboxes.

use std::process::{Command, Output, Stdio};
use std::time::Duration;

use spec::spec;

#[path = "../src/test_temp.rs"]
mod test_temp;

fn run(args: &[&str]) -> (Output, String) {
    let dir = test_temp::tempdir().unwrap();
    let root = dir.path();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .args(args)
        .env_clear()
        .current_dir(root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .env("DOT_AGENT_DECK_CONFIG", root.join("deck.toml"))
        .env("DOT_AGENT_DECK_SOCKET", root.join("hook.sock"))
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", root.join("attach.sock"))
        .env("DOT_AGENT_DECK_STATE_DIR", root.join("state"))
        .env("DOT_AGENT_DECK_LOG", root.join("deck.log"))
        .env("DOT_AGENT_DECK_EXPERIMENTAL", "0")
        .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
        .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "10")
        .env("OTEL_SDK_DISABLED", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No agent executable can be resolved, even if a regression attempts a launch.
    let empty_bin = root.join("bin");
    std::fs::create_dir(&empty_bin).unwrap();
    command.env("PATH", empty_bin);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime.block_on(async {
        let mut command = tokio::process::Command::from(command);
        // A timeout drops this specific child's wait future and kills it;
        // output pipes drain concurrently with the event-driven exit wait.
        command.kill_on_drop(true);
        let child = command.spawn().unwrap();
        tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .unwrap_or_else(|_| panic!("release-check command exceeded 10 seconds: {args:?}"))
            .unwrap()
    });
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !root.join("attach.sock").exists() && !root.join("hook.sock").exists(),
        "CLI must not bootstrap a daemon: {args:?}: {text}"
    );
    (output, text)
}

/// Scenario: Open release-check help and each of its six command help pages in a fresh empty sandbox. Every action in the design's matrix is discoverable without starting a daemon.
#[spec("inbox/release-check/003")]
#[test]
fn release_check_003_help_lists_all_six_actions() {
    let (output, text) = run(&["release-check", "--help"]);
    assert!(
        output.status.success(),
        "release-check help must parse, got {:?}: {text}",
        output.status.code()
    );
    for action in ["status", "run", "cancel", "forget", "export", "mode"] {
        assert!(
            text.split_whitespace().any(|word| word == action),
            "missing {action}: {text}"
        );
        let (output, text) = run(&["release-check", action, "--help"]);
        assert!(output.status.success(), "{action} help: {text}");
    }
}

/// Scenario: Invoke every release-check verb with missing or excess arguments, invalid modes, unsupported agents and unsafe release identifiers. Clap refuses each locally with its usage error before any daemon connection or file export can occur.
#[spec("inbox/release-check/003")]
#[test]
fn release_check_003_invalid_arguments_are_refused_locally() {
    let long_release = "a".repeat(65);
    let cases: Vec<Vec<&str>> = vec![
        vec![],
        vec!["unknown"],
        vec!["run"],
        vec!["run", "not-an-agent"],
        vec!["run", "pi"],
        vec!["run", "claude", "extra"],
        vec!["cancel", "extra"],
        vec!["status", "extra"],
        vec!["forget"],
        vec!["forget", "claude"],
        vec!["forget", "unknown", "2.1.313"],
        vec!["forget", "claude", "../../record"],
        vec!["forget", "claude", ""],
        vec!["forget", "claude", &long_release],
        vec!["forget", "claude", "2.1\n313"],
        vec!["forget", "claude", "é"],
        vec!["export"],
        vec!["export", "claude", "2.1.313"],
        vec!["export", "claude", "../../record", "export-dir"],
        vec!["export", "unknown", "2.1.313", "export-dir"],
        vec!["mode"],
        vec!["mode", "automatic"],
        vec!["mode", "AUTO"],
        vec!["mode", "off", "extra"],
    ];
    for args in cases {
        let mut command = vec!["release-check"];
        command.extend(args);
        let (output, text) = run(&command);
        assert_eq!(output.status.code(), Some(2), "{command:?}: {text}");
        assert!(
            text.contains("Usage:"),
            "expected argument guidance for {command:?}: {text}"
        );
        assert!(
            !text.contains("unrecognized subcommand 'release-check'"),
            "the release-check surface itself is missing: {text}"
        );
    }
}

/// Scenario: Invoke correctly formed status, run, cancel, forget, export and all three mode choices against missing test-owned endpoints and an empty record store. Valid syntax reaches a semantic result; run and cancel refuse the absent daemon without bootstrapping one.
#[spec("inbox/release-check/003")]
#[test]
fn release_check_003_valid_syntax_reaches_semantic_result_without_a_daemon() {
    for args in [
        vec!["status"],
        vec!["run", "claude"],
        vec!["run", "codex"],
        vec!["cancel"],
        vec!["forget", "claude", "2.1.313"],
        vec!["forget", "codex", "0.164.0"],
        vec!["export", "claude", "2.1.313", "export-dir"],
        vec!["mode", "auto"],
        vec!["mode", "ask"],
        vec!["mode", "off"],
    ] {
        let mut command = vec!["release-check"];
        command.extend(&args);
        let (output, text) = run(&command);
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "valid {command:?} must parse: {:?}: {text}",
            output.status.code()
        );
        if matches!(args[0], "run" | "cancel") {
            assert_eq!(
                output.status.code(),
                Some(1),
                "no daemon must refuse {command:?}: {text}"
            );
            assert!(
                !text.trim().is_empty(),
                "refusal must explain how {command:?} failed"
            );
        }
        if args[0] == "export" {
            assert_eq!(
                output.status.code(),
                Some(1),
                "no matching record must refuse export: {text}"
            );
        }
    }
}
