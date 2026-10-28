#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Real kernel trees and the installed helper; every agent is this test binary.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/editor_installation.rs"]
mod installation;
#[path = "support/editor_settlement.rs"]
mod settlement;

use serde_json::json;
use settlement::Fixture;
use spec::spec;

/// Scenario: Start the native stand-in with no liability and invoke the helper directly, below a Bash tool, below an updated agent and below a double-forked orphan, then make a real tool commit and exec the agent to another inode before invoking again. Each helper opens the sentinel editor and returns its status, and the git commit succeeds, proving the fixture's execution and readiness controls before the armed tests depend on them.
#[spec("inbox/probe/022")]
#[test]
fn probe_022_native_fixture_controls_without_an_arm() {
    let fixture = Fixture::start();
    for (index, kind) in ["owned", "tool", "updated", "orphan"].iter().enumerate() {
        let result = fixture.run(json!({"kind": kind}));
        assert_eq!(result["exit"], 37, "{kind}: {result}");
        fixture.expect_editor_opens(index + 1);
        fixture.expect_count(0, "the no-arm control never publishes a liability");
    }
    let result = fixture.run(json!({"kind": "git"}));
    assert_eq!(
        result["exit"], 0,
        "the restored tool editor makes a real commit: {result}"
    );
    fixture.expect_editor_opens(5);
    fixture.replace_agent();
    let result = fixture.run(json!({"kind": "owned"}));
    assert_eq!(result["exit"], 37, "replacement: {result}");
    fixture.expect_editor_opens(6);
}

/// Scenario: Start a native stand-in with an installed helper and publish one test liability without typing a key. A helper stopped before its arm load must leave that liability present until continued, then consume it exactly once without opening the editor; a second run proves the marker-only route does the same when the hook socket is unavailable.
#[spec("inbox/probe/021")]
#[test]
fn probe_021_owned_helpers_settle_once_after_the_arm_load() {
    if settlement::agent_process() {
        return;
    }
    for marker_only in [false, true] {
        let fixture = Fixture::start();
        fixture.publish();
        fixture.publish();
        fixture.expect_count(1, "repeat publish is idempotent");
        let paused = fixture.paused_owned(marker_only);
        paused.wait_started();
        fixture.expect_count(1, "a helper stopped before its arm load consumes nothing");
        fixture.no_editor();
        paused.resume();
        let result = fixture.result();
        assert_eq!(result["exit"], 0, "{result}");
        fixture.wait_count(0, "the owned armed helper consumes one liability");
        fixture.no_editor();
        fixture.withdraw();
        fixture.withdraw();
        fixture.expect_count(0, "repeat withdrawal never underflows");
        // Let the daemon observe both IPC/marker evidence and the process exit.
        // An acknowledged second publication is a barrier after the first exit.
        fixture.publish();
        fixture.expect_count(1, "late duplicate evidence cannot consume a new liability");
        fixture.expect_count_stays(1);
        fixture.withdraw();
        fixture.expect_count(0, "withdraw the one outstanding test liability");
        let refused = fixture.control_for(fixture.generation + 1, "publish");
        assert_ne!(
            refused["ok"], true,
            "a different generation is refused: {refused}"
        );
        fixture.expect_count(0, "a refused generation writes nothing to this arm page");
        assert!(
            std::fs::read(fixture.base.with_extension("input"))
                .unwrap()
                .is_empty(),
            "the control publishes without writing any PTY input"
        );
    }
}

/// Scenario: With a real installed helper and one outstanding liability, run helpers below a Bash tool, a new executable in the agent's install directory, a double-forked tool, and the agent after an in-place exec. Only the positively identified Bash tool may open the sentinel editor; all four leave the liability unchanged, including the orphan that no longer belongs to the agent's tree.
#[spec("inbox/probe/022")]
#[test]
fn probe_022_real_process_boundaries_preserve_the_arm() {
    let fixture = Fixture::start();
    fixture.publish();
    let tool = fixture.run(json!({"kind": "tool"}));
    assert_eq!(tool["exit"], 37, "{tool}");
    fixture.expect_editor_opens(1);
    fixture.expect_count(1, "the positively attributed tool leaves the arm alone");
    for kind in ["updated", "orphan"] {
        let result = fixture.run(json!({"kind": kind}));
        assert_eq!(result["exit"], 0, "{kind}: {result}");
        fixture.expect_editor_opens(1);
        fixture.expect_count(1, "an unrelated helper cannot consume the liability");
    }
    let owned = fixture.run(json!({"kind": "owned"}));
    assert_eq!(owned["exit"], 0, "A after the orphan: {owned}");
    fixture.wait_count(0, "the orphan leaves the liability for A's own helper");
    fixture.expect_editor_opens(1);
    fixture.withdraw();
    fixture.publish();
    fixture.replace_agent();
    let result = fixture.run(json!({"kind": "owned"}));
    assert_eq!(result["exit"], 0, "changed executable: {result}");
    fixture.expect_editor_opens(1);
    fixture.expect_count(1, "the recorded pid on a new executable is unknown");
}

/// Scenario: Publish an arm for a native Claude stand-in, then run a real git commit through its restored Bash-tool environment. The user's editor runs once for that tool, a shell that execs the agent's own executable cannot consume the arm or open another editor, and the original agent's helper then consumes the arm once without invoking the editor.
#[spec("inbox/probe/039")]
#[test]
fn probe_039_a_tool_commit_and_nested_agent_cannot_spend_the_composers_arm() {
    let fixture = Fixture::start();
    fixture.publish();
    let result = fixture.run(json!({"kind": "git"}));
    assert_eq!(result["exit"], 0, "the real tool commit succeeds: {result}");
    fixture.expect_editor_opens(1);
    fixture.expect_count(1, "a tool commit cannot spend the composer's arm");
    let result = fixture.run(json!({"kind": "nested"}));
    assert_eq!(result["exit"], 0, "nested stand-in: {result}");
    fixture.expect_editor_opens(1);
    fixture.expect_count(1, "a shell exec on F1 is still an unrelated invocation");
    let result = fixture.run(json!({"kind": "owned"}));
    assert_eq!(
        result["exit"], 0,
        "the original agent's invocation: {result}"
    );
    fixture.wait_count(0, "only the original agent consumes its liability");
    fixture.expect_editor_opens(1);
    let output = common::fixture_git(&fixture.repo, &fixture.daemon.home)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "settlement fixture commit\n"
    );
}
