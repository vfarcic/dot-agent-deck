#![cfg(feature = "e2e")]

//! L2 live-reload test for the daemon-hosted scheduler.
//! PRD #127 catalog `scheduler/reload/001` (M1.3).
//!
//! A `ReloadSchedules` control message re-reads the global `schedules.toml`
//! and diff/replaces the registered task set without restarting the daemon.
//!
//! Contract pinned by this test (implemented by the coder's M1.3 wiring): the
//! `ReloadSchedules` handler replies with `ok = true` and the names of the
//! currently-registered ENABLED tasks in `AttachResponse.agents`.

mod common;

use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

const INITIAL: &str = r#"
[[scheduled_tasks]]
name = "alpha"
cron = "0 9 * * *"
working_dir = "/tmp"
prompt = "alpha prompt"
enabled = true
"#;

// Drops `alpha`, adds `beta` — a reload must register beta and drop alpha.
const EDITED: &str = r#"
[[scheduled_tasks]]
name = "beta"
cron = "0 10 * * *"
working_dir = "/tmp"
prompt = "beta prompt"
enabled = true
"#;

/// Scenario: Start `daemon serve` with idle shutdown disabled and an initial
/// global `schedules.toml` registering one task (`alpha`). Rewrite the file to
/// drop `alpha` and add `beta`, then send the `ReloadSchedules` control
/// message over the attach socket. Assert the response is ok and that the
/// daemon's now-registered set contains `beta` and no longer contains `alpha`
/// — all without restarting the daemon.
#[spec("scheduler/reload/001")]
#[test]
fn reload_001_reload_swaps_registered_tasks() {
    // Idle disabled so the daemon stays up across the reload drive.
    let daemon = common::spawn_daemon_serve(Some(INITIAL), "0");

    // Edit the global config in place: remove alpha, add beta.
    std::fs::write(&daemon.schedules_path, EDITED).expect("rewrite schedules.toml");

    // Live reload over the socket — no restart.
    let resp = daemon
        .send_attach_request(&AttachRequest::ReloadSchedules)
        .expect("send ReloadSchedules");
    assert!(resp.ok, "reload failed: {:?}", resp.error);

    let registered = resp.agents.unwrap_or_default();
    assert!(
        registered.iter().any(|n| n == "beta"),
        "beta should be registered after reload, got {registered:?}"
    );
    assert!(
        !registered.iter().any(|n| n == "alpha"),
        "alpha should be gone after reload, got {registered:?}"
    );
}
