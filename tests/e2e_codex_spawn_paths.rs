#![cfg(feature = "e2e")]

//! Synthetic recorder coverage for Wrapper-strategy launch paths.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).expect("write recorder executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod recorder executable");
}

#[cfg(unix)]
fn recorder_path(record: &Path) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("recorder bin tempdir");
    write_executable(
        &dir.path().join("dot-agent-deck"),
        "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$CODEX_PATH_RECORD\"\nexec cat\n",
    );
    write_executable(
        &dir.path().join("codex"),
        "#!/bin/sh\nprintf 'BARE codex %s\\n' \"$*\" >> \"$CODEX_PATH_RECORD\"\nexec cat\n",
    );
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").expect("test runner PATH")
    );
    std::fs::write(record, "").expect("initialize launch record");
    (dir, path)
}

fn wait_for_launch(record: &Path) -> String {
    assert!(
        common::wait_for_file_substr_count(record, "codex", 1, Duration::from_secs(10)),
        "the Codex recorder was never launched"
    );
    std::fs::read_to_string(record).expect("read Codex launch record")
}

fn assert_only_wrapped(record: &Path) {
    let launched = wait_for_launch(record);
    assert!(
        launched
            .lines()
            .all(|line| line == "WRAPPED wrap --agent codex -- codex"),
        "every Codex launch on this path must cross the Wrapper strategy exactly once; observed:\n{launched}"
    );
}

fn open_form(deck: &TuiDeck) {
    deck.wait_for_string("No active sessions");
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
}

/// Scenario: Restore a persisted plain pane whose user-facing command is bare
/// `codex`, with recorder binaries ahead of PATH. The restore spawn must execute
/// exactly `dot-agent-deck wrap --agent codex -- codex`, never bare Codex.
#[spec("codex/spawn/001")]
#[test]
#[cfg(unix)]
fn spawn_001_plain_restore_wraps_codex() {
    let fixture = tempfile::tempdir().expect("plain restore record dir");
    let record = fixture.path().join("plain-restore.log");
    let (_bin, path) = recorder_path(&record);
    let _deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_continue_session("restored-codex", "codex")
        .launch_with_fixture("minimal");
    assert_only_wrapped(&record);
}

/// Scenario: Select a configured workload mode through the normal new-pane UI
/// while the form's bare command is `codex`. The mode shell-injection path must
/// transform that command through the Wrapper strategy before launch.
#[spec("codex/spawn/002")]
#[test]
#[cfg(unix)]
fn spawn_002_mode_pane_wraps_codex() {
    let fixture = tempfile::tempdir().expect("mode record dir");
    let record = fixture.path().join("mode.log");
    let (_bin, path) = recorder_path(&record);
    let deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .launch_with_fixture("codex-spawn-paths");
    open_form(&deck);
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.send_keys(b"codex");
    deck.send_keys(b"\r");
    assert_only_wrapped(&record);
}

/// Scenario: Select a configured orchestration through the normal new-pane UI
/// whose start role command is bare `codex`. The orchestration role spawn must
/// execute the registry Wrapper command rather than launching Codex directly.
#[spec("codex/spawn/003")]
#[test]
#[cfg(unix)]
fn spawn_003_orchestration_role_wraps_codex() {
    let fixture = tempfile::tempdir().expect("orchestration record dir");
    let record: PathBuf = fixture.path().join("orchestration.log");
    let (_bin, path) = recorder_path(&record);
    let deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .launch_with_fixture("codex-spawn-paths");
    open_form(&deck);
    deck.send_keys(b"\x1b[C\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    assert_only_wrapped(&record);
}

/// Scenario: Restore a persisted mode-backed pane whose saved user-facing
/// command is bare `codex`. Rebuilding the mode tab must wrap that command
/// before injecting it into the restored mode agent pane.
#[spec("codex/spawn/004")]
#[test]
#[cfg(unix)]
fn spawn_004_mode_restore_wraps_codex() {
    let fixture = tempfile::tempdir().expect("mode restore record dir");
    let record = fixture.path().join("mode-restore.log");
    let (_bin, path) = recorder_path(&record);
    let _deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_continue_mode_session("restored-mode-codex", "codex", "wrapped-mode")
        .launch_with_fixture("codex-spawn-paths");
    assert_only_wrapped(&record);
}
