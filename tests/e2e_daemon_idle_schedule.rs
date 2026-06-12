#![cfg(feature = "e2e")]

//! L2 idle-shutdown carve-out test for the daemon-hosted scheduler.
//! PRD #127 catalog `lifecycle/daemon-idle/003` (M1.4).
//!
//! Drives the real `dot-agent-deck daemon serve` process headlessly (no PTY —
//! there is no TUI surface). A registered ENABLED schedule must become a third
//! keep-alive condition so the daemon does not idle-GC itself between fires;
//! once the last enabled schedule is gone the normal idle shutdown resumes.

mod common;

use std::time::Duration;

use spec::spec;

const KEEPALIVE_SCHEDULE: &str = r#"
[[scheduled_tasks]]
name = "keepalive"
cron = "0 9 * * *"
working_dir = "/tmp"
command = "cat"
prompt = "keep the daemon alive between fires"
enabled = true
"#;

/// Scenario: Start `daemon serve` with a fast 2-second idle window and a
/// single ENABLED schedule in the global `schedules.toml`, with zero TUI
/// clients ever attaching and zero agents spawned — so the idle gate's
/// `clients == 0 && live_count == 0` holds immediately (the before-first-fire
/// and after-agent-exit gaps). Assert the daemon process is still alive well
/// past the idle window (the carve-out keeps it up). Then clear the config and
/// `dot-agent-deck schedule reload`: with no enabled schedule remaining the
/// carve-out drops and the daemon idle-exits as normal.
#[spec("lifecycle/daemon-idle/003")]
#[test]
fn daemon_idle_003_enabled_schedule_blocks_idle_exit() {
    // Idle window of 2s; the enabled schedule must override it.
    let mut daemon = common::spawn_daemon_serve(Some(KEEPALIVE_SCHEDULE), "2");

    // Comfortably longer than the idle window: with the carve-out the daemon
    // stays up because a registered enabled schedule means
    // `no_pending_schedules` is false.
    daemon.assert_alive_for(Duration::from_secs(6));

    // Drop the last enabled schedule and trigger a live reload (no restart).
    std::fs::write(&daemon.schedules_path, "").expect("clear schedules.toml");
    let out = daemon.run_schedule_cli(&["reload"]);
    assert!(
        out.status.success(),
        "schedule reload failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // With zero enabled schedules, zero clients and zero agents, the idle gate
    // now holds and the daemon must exit within the idle window plus margin.
    assert!(
        daemon.wait_for_exit(Duration::from_secs(10)),
        "daemon should idle-exit once no enabled schedule keeps it alive"
    );
}
