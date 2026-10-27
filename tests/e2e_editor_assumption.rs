#![cfg(feature = "e2e")]

//! PRD #1666 part 3 (Q35-13): the e2e editor-probe assumption through its
//! real environment reader (`src/doorbell_activation.rs`) and the daemon's
//! startup capture of it (`src/composer_probe/install.rs`). Each case reruns
//! this test as a child process with the variable set, so no test changes its
//! own environment and every capture is the child's first.

use dot_agent_deck::composer_probe::install::{
    PaneClass, ProbeAgent, assumed_classes, capture_assumption, captured_assumption,
};
use dot_agent_deck::doorbell_activation::{ASSUME_EDITOR_PROBE_ENV, read_assumed_editor_probes};
use dot_agent_deck::doorbell_screen::standin_editor_probe;
use dot_agent_deck::event::AgentType;
use semver::Version;
use spec::spec;

const CHILD: &str = "DAD_EDITOR_ASSUMPTION_CHILD";

/// Rerun `name` in a child with the assumption variable set to `value`, or
/// unset for `None`, telling it what to expect.
fn in_child(name: &str, value: Option<&str>, expect: &str) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, expect)
        .env_remove(ASSUME_EDITOR_PROBE_ENV);
    if let Some(value) = value {
        command.env(ASSUME_EDITOR_PROBE_ENV, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{value:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("checked-in-child"),
        "{value:?}: the child ran no check"
    );
}

/// Scenario: In a child process with the variable naming Claude 2.1.310 and Codex 0.164.0, read the assumption through the real reader and capture it as the daemon does at start: both give those two releases with the stand-ins' records, a later capture returns the first, and the agents roll out Claude's direct, launcher and wrapper classes and Codex's direct and launcher classes. With the variable unset, or empty, the reader assumes nothing and refuses nothing.
#[spec("inbox/probe/066")]
#[test]
fn probe_066_a_valid_assumption_is_read_and_captured_once() {
    let name = "probe_066_a_valid_assumption_is_read_and_captured_once";
    match std::env::var(CHILD).as_deref() {
        Ok("valid") => {
            let expected = vec![
                (
                    AgentType::ClaudeCode,
                    Version::new(2, 1, 310),
                    standin_editor_probe(&AgentType::ClaudeCode).unwrap(),
                ),
                (
                    AgentType::Codex,
                    Version::new(0, 164, 0),
                    standin_editor_probe(&AgentType::Codex).unwrap(),
                ),
            ];
            assert!(captured_assumption().is_empty(), "nothing before capture");
            assert_eq!(read_assumed_editor_probes(), Ok(expected.clone()));
            assert_eq!(capture_assumption(), expected.as_slice());
            assert_eq!(captured_assumption(), expected.as_slice());
            assert!(std::ptr::eq(capture_assumption(), captured_assumption()));
            assert_eq!(
                assumed_classes(captured_assumption()),
                [
                    PaneClass::Direct(ProbeAgent::Claude),
                    PaneClass::Launcher(ProbeAgent::Claude),
                    PaneClass::Wrapper,
                    PaneClass::Direct(ProbeAgent::Codex),
                    PaneClass::Launcher(ProbeAgent::Codex),
                ]
            );
            println!("checked-in-child");
        }
        Ok("none") => {
            assert_eq!(read_assumed_editor_probes(), Ok(vec![]));
            assert!(capture_assumption().is_empty());
            println!("checked-in-child");
        }
        Ok(other) => panic!("unknown expectation {other:?}"),
        Err(_) => {
            in_child(name, Some("claude@2.1.310, codex@0.164.0"), "valid");
            in_child(name, None, "none");
            in_child(name, Some(""), "none");
        }
    }
}

/// Scenario: In a child process per list, set the variable to a malformed list, an entry named twice, a release with no record, another agent's release, a release with build metadata, and a known entry beside an unknown one. The real reader refuses each with its reason, and the startup capture assumes nothing, so no class rolls out.
#[spec("inbox/probe/066")]
#[test]
fn probe_066_an_invalid_assumption_is_refused_by_the_reader() {
    let name = "probe_066_an_invalid_assumption_is_refused_by_the_reader";
    if std::env::var_os(CHILD).is_some() {
        let refused = read_assumed_editor_probes();
        assert!(refused.is_err(), "{refused:?}");
        assert!(capture_assumption().is_empty());
        assert!(assumed_classes(captured_assumption()).is_empty());
        println!("checked-in-child");
        return;
    }
    for list in [
        "broken",
        "claude@2.1.310,claude@2.1.310",
        "claude@99.0.0",
        "codex@2.1.310",
        "claude@2.1.310+metadata",
        "claude@2.1.310,opencode@1.0.0",
    ] {
        in_child(name, Some(list), "refused");
    }
}
