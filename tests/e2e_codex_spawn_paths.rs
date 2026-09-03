#![cfg(all(feature = "e2e", feature = "e2e-live"))]

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
/// Returns the recorder tempdir, a `$PATH` with it in front, and the path of its
/// fake `dot-agent-deck`.
///
/// The third element is what makes these tests work now that the wrapper rewrite
/// names the co-located build by absolute path instead of a bare `dot-agent-deck`
/// for `$PATH` to resolve (so the suite tests the build it compiled, not whatever
/// is installed). PATH interception no longer reaches the rewrite; the deck needs
/// `DOT_AGENT_DECK_WRAP_BIN` pointed at the recorder instead.
fn recorder_path(record: &Path) -> (tempfile::TempDir, String, PathBuf) {
    let dir = common::harness_tempdir().expect("recorder bin tempdir");
    write_executable(
        &dir.path().join("dot-agent-deck"),
        "#!/bin/sh\nprintf 'WRAPPED %s\\n' \"$*\" >> \"$CODEX_PATH_RECORD\"\nexec cat\n",
    );
    write_executable(
        &dir.path().join("codex"),
        "#!/bin/sh\nprintf 'BARE codex %s\\n' \"$*\" >> \"$CODEX_PATH_RECORD\"\nexec cat\n",
    );
    write_executable(
        &dir.path().join("devbox"),
        "#!/bin/sh\nprintf 'BARE devbox %s\\n' \"$*\" >> \"$CODEX_PATH_RECORD\"\nexec cat\n",
    );
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").expect("test runner PATH")
    );
    std::fs::write(record, "").expect("initialize launch record");
    let wrap_bin = dir.path().join("dot-agent-deck");
    (dir, path, wrap_bin)
}

fn wait_for_launch(record: &Path) -> Vec<String> {
    assert!(
        common::wait_for_file_substr_count(record, "WRAPPED", 1, Duration::from_secs(10)),
        "the Codex recorder was never launched"
    );
    // PRD #20 §4.2.1: startup scoped-trust runs `codex app-server` to read Codex's
    // own hook hashes, which the PATH recorder also logs. That probe is not an
    // agent launch, so the launch-path assertion looks past it.
    common::recorded_agent_launches(record)
}

fn assert_only_wrapped(record: &Path) {
    let launched = wait_for_launch(record);
    assert!(
        launched
            .iter()
            .all(|line| line == "WRAPPED wrap --agent codex -- codex"),
        "every Codex launch on this path must cross the Wrapper strategy exactly once; observed:\n{}",
        launched.join("\n")
    );
}

fn wait_for_declared_launcher(record: &Path) -> Vec<String> {
    common::wait_for_file_containing(record, "devbox run codex-big", Duration::from_secs(10))
        .unwrap_or_else(|state| panic!("the declared launcher was never executed: {state}"));
    common::recorded_agent_launches(record)
}

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bin_dir = Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default())
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
    let fixture = common::harness_tempdir().expect("plain restore record dir");
    let record = fixture.path().join("plain-restore.log");
    let (_bin, path, wrap_bin) = recorder_path(&record);
    let _deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_env("DOT_AGENT_DECK_WRAP_BIN", wrap_bin.to_string_lossy())
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
    let fixture = common::harness_tempdir().expect("mode record dir");
    let record = fixture.path().join("mode.log");
    let (_bin, path, wrap_bin) = recorder_path(&record);
    let deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_env("DOT_AGENT_DECK_WRAP_BIN", wrap_bin.to_string_lossy())
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
    let fixture = common::harness_tempdir().expect("orchestration record dir");
    let record: PathBuf = fixture.path().join("orchestration.log");
    let (_bin, path, wrap_bin) = recorder_path(&record);
    let deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_env("DOT_AGENT_DECK_WRAP_BIN", wrap_bin.to_string_lossy())
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
    let fixture = common::harness_tempdir().expect("mode restore record dir");
    let record = fixture.path().join("mode-restore.log");
    let (_bin, path, wrap_bin) = recorder_path(&record);
    let _deck = TuiDeck::builder()
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_env("DOT_AGENT_DECK_WRAP_BIN", wrap_bin.to_string_lossy())
        .with_continue_mode_session("restored-mode-codex", "codex", "wrapped-mode")
        .launch_with_fixture("codex-spawn-paths");
    assert_only_wrapped(&record);
}

/// Scenario: Open an orchestration whose start role declares Codex but runs
/// through a non-inferable launcher. Without delegating a task or synthesizing a
/// hook, the role must launch through the Codex wrapper and its card must read Codex.
#[spec("codex/spawn/009")]
#[test]
#[cfg(unix)]
fn spawn_009_declared_orchestration_launcher_wraps_and_badges_codex() {
    let fixture = common::harness_tempdir().expect("declared orchestration record dir");
    let record = fixture.path().join("declared-orchestration.log");
    let (_bin, path, wrap_bin) = recorder_path(&record);
    let deck = TuiDeck::builder()
        .with_pty_size(160, 42)
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_env("DOT_AGENT_DECK_WRAP_BIN", wrap_bin.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    std::fs::write(
        deck.workdir().join(".dot-agent-deck.toml"),
        "[[orchestrations]]\n\
         name = \"declared-codex\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"recorder\"\n\
         command = \"devbox run codex-big\"\n\
         agent = \"codex\"\n\
         start = true\n\
         clear = false\n",
    )
    .expect("write declared orchestration config");

    open_form(&deck);
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");

    let launched = wait_for_declared_launcher(&record);
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    let codex_badge = deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(5));
    let grid = deck.snapshot_grid();

    assert!(
        launched == ["WRAPPED wrap --agent codex -- devbox run codex-big"] && codex_badge,
        "a declared Codex role must wrap its non-inferable launcher exactly once and render a Codex badge before any delegate or hook event; launches={launched:?}, codex_badge={codex_badge}\nFinal grid:\n{grid}"
    );
}

/// Scenario: Select a configured mode whose agent pane declares Codex, then
/// enter a non-inferable launcher in the form. The shell-injected command must
/// be wrapped exactly once and the pane's Dashboard card must read Codex.
#[spec("codex/spawn/011")]
#[test]
#[cfg(unix)]
fn spawn_011_declared_mode_launcher_wraps_and_badges_codex() {
    let fixture = common::harness_tempdir().expect("declared mode record dir");
    let record = fixture.path().join("declared-mode.log");
    let (_bin, path, wrap_bin) = recorder_path(&record);
    let deck = TuiDeck::builder()
        .with_pty_size(160, 42)
        .with_env("PATH", path)
        .with_env("CODEX_PATH_RECORD", record.to_string_lossy())
        .with_env("DOT_AGENT_DECK_WRAP_BIN", wrap_bin.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    std::fs::write(
        deck.workdir().join(".dot-agent-deck.toml"),
        "[[modes]]\n\
         name = \"declared-codex-mode\"\n\
         agent = \"codex\"\n\
         reactive_panes = 0\n",
    )
    .expect("write declared mode config");

    open_form(&deck);
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.send_keys(b"devbox run codex-big");
    deck.send_keys(b"\r");

    let launched = wait_for_declared_launcher(&record);
    deck.send_bytes(b"\x04");
    deck.send_bytes(b"\x1b[D");
    deck.wait_for_string("session(s)");
    let codex_badge = deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(5));
    let grid = deck.snapshot_grid();

    assert!(
        launched == ["WRAPPED wrap --agent codex -- devbox run codex-big"] && codex_badge,
        "a declared Codex mode pane must wrap its shell-injected non-inferable launcher exactly once and render a Codex Dashboard badge; launches={launched:?}, codex_badge={codex_badge}\nFinal grid:\n{grid}"
    );
}

/// Scenario: Start a real cheap-model Codex through a bespoke launcher script
/// used by an orchestration role that declares Codex. Before any prompt is
/// submitted, the role card must already read Codex and the real CLI must be live.
#[spec("codex/spawn/012")]
#[test]
#[cfg(unix)]
fn spawn_012_real_script_launched_codex_badges_before_first_prompt() {
    skip_unless!(common::check_codex_available());

    let real_codex = std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("codex"))
                .find(|candidate| candidate.is_file())
        })
        .expect("available Codex binary resolves on PATH");
    let deck = TuiDeck::builder()
        .with_pty_size(160, 42)
        .with_env("PATH", path_with_binary_dir())
        .with_env("REAL_CODEX_BIN", real_codex.to_string_lossy())
        .with_imported_codex_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let work = deck.workdir().to_path_buf();
    let launch_record = work.join("real-script-codex.log");
    write_executable(
        &work.join("run-codex.sh"),
        "#!/bin/sh\nprintf 'REAL_CODEX %s\\n' \"$*\" >> real-script-codex.log\nexec \"$REAL_CODEX_BIN\" \"$@\"\n",
    );
    let command = format!(
        "./run-codex.sh --model {} --sandbox workspace-write --ask-for-approval never -c 'sandbox_workspace_write.network_access=true' -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model(),
    );
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\n\
             name = \"real-declared-codex\"\n\n\
             [[orchestrations.roles]]\n\
             name = \"real-codex\"\n\
             command = {command:?}\n\
             agent = \"codex\"\n\
             start = true\n\
             clear = false\n"
        ),
    )
    .expect("write real declared Codex orchestration config");

    let events = deck.subscribe_events();
    open_form(&deck);
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    common::wait_for_file_containing(&launch_record, "REAL_CODEX", Duration::from_secs(20))
        .unwrap_or_else(|state| panic!("the bespoke real-Codex launcher never executed: {state}"));

    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Pane Ctrl+N]");
    let codex_badge = deck.wait_for_grid_string_within("Codex ·", Duration::from_secs(5));
    let grid = deck.snapshot_grid();
    let real_launch =
        std::fs::read_to_string(&launch_record).expect("read bespoke real-Codex launch record");
    let no_prompt_event = !events.snapshot().iter().any(|event| {
        event
            .user_prompt
            .as_deref()
            .is_some_and(|prompt| !prompt.is_empty())
    });

    assert!(
        codex_badge && no_prompt_event && grid.contains("Idle"),
        "the real script-launched Codex role must render a Codex Idle card before any prompt is submitted; real_launch={real_launch:?}, codex_badge={codex_badge}, no_prompt_event={no_prompt_event}\nFinal grid:\n{grid}"
    );
}
