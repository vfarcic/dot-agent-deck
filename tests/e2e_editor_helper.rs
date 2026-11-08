#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Synthetic helper subprocesses only: no daemon, agent or user configuration.
mod common;

use dot_agent_deck::composer_probe::install::ProbeAgent;
use dot_agent_deck::composer_probe::instance::{DaemonId, Instance};
use spec::spec;
use std::fs::File;
use std::os::unix::net::UnixListener;
use std::process::Command;

fn run(case: &str, armed: bool) {
    let dir = common::race_safe_tempdir();
    let instance = Instance::create(
        &dir.path().join("editor"),
        DaemonId::this_process().unwrap(),
        &mut File::open(env!("CARGO_BIN_EXE_dot-agent-deck")).unwrap(),
    )
    .unwrap();
    let mut generation = instance
        .generation("3", Some(ProbeAgent::Codex), None)
        .unwrap();
    generation
        .arm()
        .publisher()
        .publish_native(u32::from(armed), 17, 23);
    // The correct peer owns a real socket but never answers capabilities.
    // This exercises transport-failure notice row T without a daemon.
    let socket = dir.path().join("hook.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let output = Command::new("python3")
        .arg("-c")
        .arg(include_str!("support/editor_helper_checks.py"))
        .arg(case)
        .arg(instance.helper())
        .arg(dir.path())
        .arg(&socket)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("OTEL_SDK_DISABLED", "true")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{case}: stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Scenario: Invoke the installed helper with a present arm and an unresponsive fixture socket, with stderr captured in a pipe. It prints exactly the approved plain notice, opens no editor, and returns even when it inherits SIGALRM ignored and blocked.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_armed_pipe_notice_bytes() {
    run("armed-pipe", true);
}

/// Scenario: Invoke the armed helper on a fixture terminal with its normal newline processing, then with OPOST disabled. The raw master capture must equal the fixed notice with precisely the terminal's expected newline conversion and no editor output.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_armed_notice_raw_master_bytes() {
    run("armed-pty", true);
}

/// Scenario: Stop output on a fixture terminal before invoking an armed helper. Its blocked notice terminates through the armed alarm, without opening the user's editor or emitting terminal bytes.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_armed_notice_stopped_terminal() {
    run("armed-stopped", true);
}

/// Scenario: Fill a fixture terminal's output queue without reading its master, then invoke an armed helper. The notice cannot escape through an accidentally drained queue, and the armed alarm ends the helper without opening an editor.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_armed_notice_full_terminal() {
    run("armed-full", true);
}

/// Scenario: Run the armed helper in a background group on a fixture controlling terminal with TOSTOP. Observe its actual SIGTTOU stop and persistence past the alarm deadline, then continue it and observe the pending SIGALRM terminate it.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_armed_background_notice_stops_until_continued() {
    run("armed-background", true);
}

/// Scenario: Invoke the unarmed helper with a sentinel editor which takes eight seconds and exits 37. The helper must outlive five seconds, preserve the export argument and restored editor settings, wait for the sentinel, and return 37 without terminal output.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_ordinary_editor_outlives_armed_alarm() {
    run("editor-long", false);
}

/// Scenario: Run a silent editor which terminates itself with SIGTERM through the unarmed helper. The helper returns the same signal status and writes no diagnostic to stderr.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_signalled_editor_preserves_signal_status() {
    run("editor-signal", false);
}

/// Scenario: Run an unarmed helper whose silent editor exits 37 on stopped and full fixture terminals. Its post-editor exit must preserve 37 and add no bytes, with no dependency on terminal output progress.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_post_editor_exit_needs_no_terminal_progress() {
    run("editor-stalled", false);
}

/// Scenario: Run an unarmed helper and silent editor in a background group with TOSTOP set. The helper must return the editor's status without stopping or writing a terminal diagnostic.
#[spec("inbox/probe/020")]
#[test]
fn probe_020_post_editor_background_exit_does_not_stop() {
    run("editor-background", false);
}
