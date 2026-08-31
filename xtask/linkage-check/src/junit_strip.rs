//! Issue #785: the JUnit report the credentialed `e2e-live` job uploads must be
//! structurally incapable of carrying test output, and that property is a
//! RUNTIME one — `scripts/junit-strip-output.py` either drops the
//! output-bearing elements or it does not, and no compile step can tell.
//!
//! That is the same shape CLAUDE.md rule 5 records for
//! `xtask/linkage-check/src/clean_tmp.rs`: a tool whose safety properties are
//! runtime assertions and nothing else, which is exactly the class that sat
//! untested through #436 because "building is not linting". So these tests put
//! the stripper in `cargo test-fast`, where a change that quietly reopens the
//! sink goes red on the per-task gate.
//!
//! Three things are pinned:
//!
//! 1. the script's own `--self-test` passes (cheap, and it is what a
//!    contributor is told to run);
//! 2. an INDEPENDENT fixture — the exact element shapes nextest 0.9.143 emits
//!    for a failed-and-retried test, captured from a real report — comes out
//!    with the placeholder credential gone and the metadata intact. This half
//!    does not trust the script's self-test;
//! 3. `.github/workflows/e2e-live.yml` still uploads the STRIPPED path and not
//!    the raw report, because "simplify the upload back to
//!    `target/nextest/default/junit.xml`" is the one edit that silently undoes
//!    all of the above.
//!
//! Tests only. The rule lives in the script; this is its gate.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Workspace root, from this crate's manifest dir rather than the process cwd,
/// so the tests do not depend on how the runner was invoked.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join("scripts/junit-strip-output.py")
}

/// `python3` is on every GitHub runner and in this repo's devbox. Where it is
/// absent, say so loudly rather than failing a contributor's unrelated change —
/// the same discipline `verify_pr_stream.rs` applies to `bash`/`jq`.
fn python_present() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The placeholder that stands in for a leaked credential. NOT a real key
/// shape — the point is only that a distinctive string present in every output
/// surface of the input is absent from the output.
const PLACEHOLDER: &str = "PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY";

/// A real nextest JUnit report, captured from a probe run of one passing and
/// one failing-then-retried test with `retries = 1`, with the leaked text
/// replaced by [`PLACEHOLDER`]. Note the FIVE places the value appears: the
/// `<failure>` message attribute, the `<failure>` body, the `<rerunFailure>`
/// message attribute and body, and the `<system-out>`/`<system-err>` pairs that
/// hang off both the `<rerunFailure>` and the `<testcase>` itself. A stripper
/// that handles only `<system-out>`/`<system-err>` passes a naive test and
/// still leaks three ways, which is why the fixture is the real shape.
const NEXTEST_FAILURE_REPORT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="2" skipped="0" failures="1" errors="0" uuid="dcc8d493-5350-4767-85d1-ceef478f9807" timestamp="2026-08-31T18:42:03.739+00:00" time="0.010">
    <testsuite name="junit-probe::probe" tests="2" skipped="0" errors="0" failures="1">
        <testcase name="passing_with_output" classname="junit-probe::probe" timestamp="2026-08-31T18:42:03.740+00:00" time="0.004"/>
        <testcase name="failing_with_output" classname="junit-probe::probe" timestamp="2026-08-31T18:42:03.740+00:00" time="0.003">
            <failure message="thread &apos;failing_with_output&apos; panicked, PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY" type="test failure with exit code 101">panicked carrying PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY</failure>
            <rerunFailure timestamp="2026-08-31T18:42:03.745+00:00" time="0.004" message="thread &apos;failing_with_output&apos; panicked, PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY" type="test failure with exit code 101">panicked carrying PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY
                <system-out>PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY on stdout</system-out>
                <system-err>PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY on stderr</system-err>
            </rerunFailure>
            <system-out>PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY on stdout</system-out>
            <system-err>PLACEHOLDER-CREDENTIAL-NOT-A-REAL-KEY on stderr</system-err>
        </testcase>
    </testsuite>
</testsuites>
"#;

/// The script's own `--self-test` must pass. It is what the workflow comment
/// and `docs/develop/e2e-lanes.md` both tell a reader to run, so a self-test
/// that has rotted is worse than none.
#[test]
fn junit_strip_self_test_passes() {
    if !python_present() {
        eprintln!("SKIP: junit-strip-output self-test needs `python3` on PATH");
        return;
    }
    let out = Command::new("python3")
        .arg(script())
        .arg("--self-test")
        .output()
        .expect("could not run scripts/junit-strip-output.py");
    assert!(
        out.status.success(),
        "scripts/junit-strip-output.py --self-test failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// The load-bearing assertion: a real failure-bearing report goes in, and what
/// comes out carries no credential material and no free text — while still
/// carrying everything issue #564 uploads the file for.
#[test]
fn a_real_failure_report_loses_every_output_surface() {
    if !python_present() {
        eprintln!("SKIP: junit-strip-output fixture test needs `python3` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("junit.xml");
    let output = dir.path().join("metadata.xml");
    std::fs::write(&input, NEXTEST_FAILURE_REPORT).expect("write fixture");

    let out = Command::new("python3")
        .arg(script())
        .arg(&input)
        .arg(&output)
        .output()
        .expect("could not run scripts/junit-strip-output.py");
    assert!(
        out.status.success(),
        "stripping the fixture failed:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );

    let stripped = std::fs::read_to_string(&output).expect("stripper wrote no output");

    assert!(
        !stripped.contains(PLACEHOLDER),
        "the placeholder credential survived the strip:\n{stripped}"
    );
    for banned in [
        "system-out",
        "system-err",
        "message=",
        "panicked",
        "stdout",
        "stderr",
    ] {
        assert!(
            !stripped.contains(banned),
            "`{banned}` survived the strip, so the report can still carry free text:\n{stripped}"
        );
    }

    // ...and the metadata #564 wants is all still there.
    for wanted in [
        r#"name="passing_with_output""#,
        r#"name="failing_with_output""#,
        r#"classname="junit-probe::probe""#,
        r#"type="test failure with exit code 101""#,
        r#"time="0.003""#,
        r#"tests="2""#,
        r#"failures="1""#,
        "<rerunFailure",
    ] {
        assert!(
            stripped.contains(wanted),
            "metadata `{wanted}` was lost, so the artifact no longer serves #564:\n{stripped}"
        );
    }
}

/// A missing input is a no-op, not an error. The workflow step runs under
/// `if: always()` and is reached on paths where the tests never produced a
/// report; failing there would redden a job for a non-problem, and writing a
/// file anyway would upload something meaningless.
#[test]
fn a_missing_report_is_a_no_op() {
    if !python_present() {
        eprintln!("SKIP: junit-strip-output missing-input test needs `python3` on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("metadata.xml");
    let out = Command::new("python3")
        .arg(script())
        .arg(dir.path().join("absent.xml"))
        .arg(&output)
        .output()
        .expect("could not run scripts/junit-strip-output.py");
    assert!(
        out.status.success(),
        "a missing input must exit 0:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !output.exists(),
        "a missing input must write nothing, so `if-no-files-found` uploads nothing"
    );
}

/// The coupling that undoes everything above if it drifts: the credentialed job
/// must upload the STRIPPED path, and must not name the raw report in its
/// `upload-artifact` step.
#[test]
fn the_live_workflow_uploads_the_stripped_report() {
    let workflow = repo_root().join(".github/workflows/e2e-live.yml");
    let text = std::fs::read_to_string(&workflow)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", workflow.display()));

    assert!(
        text.contains("scripts/junit-strip-output.py"),
        "e2e-live.yml no longer runs the JUnit stripper; the artifact it uploads can then \
         carry ANTHROPIC_API_KEY (issue #785)"
    );
    assert!(
        text.contains("path: ${{ runner.temp }}/junit-metadata.xml"),
        "e2e-live.yml's upload step no longer points at the stripped report"
    );
    assert!(
        !text.contains("path: target/nextest/default/junit.xml"),
        "e2e-live.yml uploads the RAW nextest report again. nextest stores failed and retried \
         tests' stdout/stderr in it, that file is written by the process holding \
         ANTHROPIC_API_KEY, and GitHub's secret masking does not cover uploaded artifacts on a \
         public repository (issue #785). Upload the stripped copy."
    );
}
