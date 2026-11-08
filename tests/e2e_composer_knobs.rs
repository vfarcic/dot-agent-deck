#![cfg(all(feature = "e2e", unix))]

use spec::spec;

/// Scenario: Start the three synthetic composer screens directly on private PTYs, without a deck or real agent. Exercise their default-off controls, editor restoration and atomic command/result handoff, including a replacement paused at the parse boundary.
#[spec("harness/composer/001")]
#[test]
fn composer_001_fixture_knobs_and_atomic_tool_handoff() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/composer_knob_checks.py"
        ))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("synthetic composer controls require python3 on PATH");
    assert!(
        output.status.success(),
        "composer control checks failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Scenario: Run each synthetic composer on a private PTY and open its external editor with Ctrl+G. Check the exported draft, suspend/resume bytes, editor selection, failure preservation and each deliberate input-loss or delayed-editor control without starting a deck or real agent.
#[spec("harness/composer/001")]
#[test]
fn composer_001_external_editor_and_input_mechanisms() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/composer_editor_checks.py"
        ))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("synthetic external editor checks require python3 on PATH");
    assert!(
        output.status.success(),
        "composer external editor checks failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
