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

use std::time::Duration;

use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

const INITIAL: &str = r#"
[[scheduled_tasks]]
name = "alpha"
cron = "0 9 * * *"
working_dir = "/tmp"
command = "cat"
prompt = "alpha prompt"
enabled = true
"#;

// Drops `alpha`, adds `beta` — a reload must register beta and drop alpha.
const EDITED: &str = r#"
[[scheduled_tasks]]
name = "beta"
cron = "0 10 * * *"
working_dir = "/tmp"
command = "cat"
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

// Two distinctive prompt markers so the delivered-prompt assertion can tell the
// stale (pre-edit) value from the new (post-edit) one without ambiguity.
const PROMPT_ALPHA: &str = "PROMPT_ALPHA";
const PROMPT_BRAVO: &str = "PROMPT_BRAVO";

/// Build one single-agent `[[scheduled_tasks]]` block with a FIXED name and
/// cron — only `prompt` varies — so a rewrite is a prompt-ONLY edit. `command`
/// is `cat`, which echoes the delivered prompt back onto the PTY so the test can
/// observe which prompt reached the spawned agent. The cron never fires during
/// the test window; fires are driven explicitly via run-now.
fn prompt_only_task(working_dir: &str, prompt: &str) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"gamma\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"{working_dir}\"\n\
         command = \"cat\"\n\
         prompt = \"{prompt}\"\n\
         enabled = true\n"
    )
}

/// Scenario: Start `daemon serve` with idle shutdown disabled and an initial
/// global `schedules.toml` registering one single-agent task (`gamma`) whose
/// prompt is `PROMPT_ALPHA`. Rewrite the file changing ONLY the prompt to
/// `PROMPT_BRAVO` (same name, same cron) and send `ReloadSchedules`, then fire
/// the task via run-now. Assert exactly one agent spawns and its PTY echoes the
/// NEW prompt `PROMPT_BRAVO` and never the stale `PROMPT_ALPHA` — proving a
/// prompt-only edit is honored on the next fire (it is NOT today: reload diffs
/// on the cron only, so the live callback keeps the prompt captured at first
/// registration).
#[spec("scheduler/reload/002")]
#[test]
fn reload_002_prompt_only_edit_delivers_new_prompt() {
    // A fresh working_dir with no `.dot-agent-deck.toml` → single-agent card.
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let work = scratch.path().join("work");
    std::fs::create_dir_all(&work).expect("create work dir");
    let work_str = work.to_string_lossy().into_owned();

    // Idle disabled so the daemon stays up across the reload + fire drive.
    let daemon = common::spawn_daemon_serve(Some(&prompt_only_task(&work_str, PROMPT_ALPHA)), "0");

    // Prompt-ONLY edit: same name, same cron, new prompt. (Mirrors reload_001's
    // in-place rewrite + ReloadSchedules, but deliberately keeps the cron fixed
    // so this isolates the prompt-update path.)
    std::fs::write(
        &daemon.schedules_path,
        prompt_only_task(&work_str, PROMPT_BRAVO),
    )
    .expect("rewrite schedules.toml");

    let resp = daemon
        .send_attach_request(&AttachRequest::ReloadSchedules)
        .expect("send ReloadSchedules");
    assert!(resp.ok, "reload failed: {:?}", resp.error);

    // Fire the task now — no cron wait.
    daemon.run_now("gamma").expect("run-now gamma");

    // The fire path itself still works: exactly one agent spawns. This isolates
    // the bug to the prompt CONTENT, not the spawn/registration plumbing.
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(
        records.len(),
        1,
        "the prompt-only-edited task must still fire and spawn exactly one agent, got {records:?}"
    );
    let agent = &records[0];

    // The NEW prompt must reach the spawned agent.
    assert!(
        daemon.attach_and_wait_for_output(&agent.id, PROMPT_BRAVO, Duration::from_secs(10)),
        "after a prompt-only edit + reload, the fire must deliver the NEW prompt \
         {PROMPT_BRAVO}, not the value captured at first registration"
    );

    // ...and the stale pre-edit prompt must NOT.
    assert!(
        !daemon.attach_and_wait_for_output(&agent.id, PROMPT_ALPHA, Duration::from_secs(2)),
        "the fire must NOT deliver the stale prompt {PROMPT_ALPHA} after the prompt was edited"
    );
}
