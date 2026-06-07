#![cfg(feature = "e2e")]

//! L2 tab-lifecycle tests for the daemon-hosted scheduler (PRD #127 Phase 2B,
//! M2.2): reuse-by-default, `new_tab_per_fire` opt-in, and mid-interaction
//! deliver-on-idle.
//!
//! Observed only through externally-visible behavior — the daemon's agent
//! registry (`ListAgents`, agent count) and on-disk delivery records — driven
//! by the existing `RunNow` control message. No internal reuse API is
//! referenced.
//!
//! ## Delivery observation
//! Each task's `command` is a tiny "recorder" shell loop that appends every
//! line it reads on stdin to a per-test file. Because the daemon delivers the
//! prompt by writing `prompt + CR` into the PTY, each delivery produces exactly
//! ONE recorded line containing the prompt marker — immune to PTY echo doubling
//! (a bare `cat` would echo each line twice: once via the tty, once via its own
//! stdout). User keystrokes injected via STREAM_IN are recorded too, but carry
//! a different marker so they don't inflate the delivery count.
//!
//! ## Skip-if-running
//! A fire's callback stays "running" through the ~300ms delivery buffer, so a
//! second fire issued too soon is SKIPPED (Phase 1 skip-if-prior-run-active).
//! Each test therefore waits for the prior delivery to be RECORDED before
//! firing again — mirroring real cron fires, which never overlap this tightly.
//!
//! ## DEBOUNCE INJECTION CONTRACT (pinned for the coder)
//! The deliver-on-idle debounce window is overridable via the
//! `DOT_AGENT_DECK_REUSE_DEBOUNCE_MS` environment variable (milliseconds), so
//! reuse/003 runs fast without a real ~5s wait. It sets the window to 2000.

mod common;

use std::time::Duration;

use spec::spec;

const PROMPT_MARKER: &str = "REUSEPROMPTMARKER";
const TYPING_MARKER: &str = "USERTYPINGMARKER";

/// Create a working dir plus a "recorder" command that appends every stdin line
/// to `record_file`. Returns (working_dir, record_file, command-string). The
/// command is a bare path to a +x script so it needs no TOML quoting.
fn recorder_setup(
    base: &std::path::Path,
    tag: &str,
) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let work = base.join(format!("work-{tag}"));
    std::fs::create_dir_all(&work).expect("create work dir");
    let record_file = base.join(format!("record-{tag}.log"));
    let script = base.join(format!("recorder-{tag}.sh"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nwhile IFS= read -r l; do echo \"$l\" >> \"{}\"; done\n",
            record_file.to_string_lossy()
        ),
    )
    .expect("write recorder script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod recorder script");
    }
    (work, record_file, script.to_string_lossy().into_owned())
}

/// One `[[scheduled_tasks]]` block. The cron never fires on its own during the
/// test window; tests trigger fires via run-now.
fn task_block(name: &str, working_dir: &str, command: &str, new_tab_per_fire: bool) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"{name}\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{working_dir}\"\n\
         command = \"{command}\"\n\
         prompt = \"{PROMPT_MARKER}\"\n\
         new_tab_per_fire = {new_tab_per_fire}\n\
         enabled = true\n\n"
    )
}

/// Scenario: Register a `new_tab_per_fire = false` (default) task and fire it
/// twice via run-now (the second fire only after the first delivery is
/// recorded, so it isn't skipped). Assert the daemon reuses one tab — the agent
/// count never grows to 2 — and that the second fire delivers the prompt AGAIN
/// (two recorded prompt lines), proving the prompt re-entered the existing tab.
#[spec("scheduler/reuse/001")]
#[test]
fn reuse_001_default_reuses_one_tab() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let (work, record, command) = recorder_setup(scratch.path(), "reuse");

    let toml = task_block("reuse", &work.to_string_lossy(), &command, false);
    let daemon = common::spawn_daemon_serve(Some(&toml), "0");

    // First fire spawns the one tab; wait for its delivery to be recorded so
    // the callback has finished (next fire won't be skipped).
    daemon.run_now("reuse").expect("run-now reuse #1");
    daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert!(
        common::wait_for_file_substr_count(&record, PROMPT_MARKER, 1, Duration::from_secs(10)),
        "first fire must deliver the prompt once"
    );

    // Second fire must REUSE, not spawn — the agent count must never reach 2.
    daemon.run_now("reuse").expect("run-now reuse #2");
    daemon.assert_agent_count_stays_at_most(1, Duration::from_secs(3));

    // ...and the second prompt must have been delivered into that one tab.
    assert!(
        common::wait_for_file_substr_count(&record, PROMPT_MARKER, 2, Duration::from_secs(10)),
        "a reuse fire must re-deliver the prompt into the existing tab (expected two recorded prompts)"
    );
}

/// Scenario: Register a `new_tab_per_fire = true` task and fire it twice (the
/// second only after the first delivery is recorded). Assert two distinct tabs
/// are opened (agent count 1 → 2, distinct pane ids) and both fires delivered
/// the prompt.
#[spec("scheduler/reuse/002")]
#[test]
fn reuse_002_new_tab_per_fire_opens_distinct_tabs() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let (work, record, command) = recorder_setup(scratch.path(), "fresh");

    let toml = task_block("fresh", &work.to_string_lossy(), &command, true);
    let daemon = common::spawn_daemon_serve(Some(&toml), "0");

    daemon.run_now("fresh").expect("run-now fresh #1");
    let first = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(first.len(), 1, "first fire spawns one tab");
    assert!(
        common::wait_for_file_substr_count(&record, PROMPT_MARKER, 1, Duration::from_secs(10)),
        "first fire must deliver the prompt"
    );

    daemon.run_now("fresh").expect("run-now fresh #2");
    let both = daemon.wait_for_agent_count(2, Duration::from_secs(10));
    assert_eq!(
        both.len(),
        2,
        "new_tab_per_fire = true must open a second, distinct tab, got {both:?}"
    );

    let mut ids: Vec<String> = both.iter().map(|r| r.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 2, "the two fires must be distinct panes");

    // Both fires delivered their prompt.
    assert!(
        common::wait_for_file_substr_count(&record, PROMPT_MARKER, 2, Duration::from_secs(10)),
        "both per-fire tabs must receive the prompt"
    );
}

/// Scenario: With a short injected debounce (`DOT_AGENT_DECK_REUSE_DEBOUNCE_MS`
/// = 2000), fire a `new_tab_per_fire = false` task once to open the reused
/// pane, simulate a recent user keystroke into it, then fire again. The reuse
/// prompt must be QUEUED — not delivered while the user is "typing" (within the
/// debounce window) — and then delivered once the pane goes idle. A subsequent
/// fire with no recent input is delivered immediately.
#[spec("scheduler/reuse/003")]
#[test]
fn reuse_003_deliver_on_idle_debounce() {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let (work, record, command) = recorder_setup(scratch.path(), "idle");

    let toml = task_block("idle", &work.to_string_lossy(), &command, false);
    let daemon = common::spawn_daemon_serve_with_env(
        Some(&toml),
        "0",
        &[("DOT_AGENT_DECK_REUSE_DEBOUNCE_MS", "2000")],
    );

    // First fire opens the reused pane and delivers the prompt once.
    daemon.run_now("idle").expect("run-now idle #1");
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    let pane = records.first().cloned().expect("first fire spawns a pane");
    assert!(
        common::wait_for_file_substr_count(&record, PROMPT_MARKER, 1, Duration::from_secs(10)),
        "first fire must deliver the prompt once"
    );

    // Simulate the user actively typing into the pane (sets the debounce clock).
    assert!(
        daemon.send_pane_input(&pane.id, TYPING_MARKER),
        "simulated user keystroke must reach the pane"
    );

    // Reuse fire while the user is "typing": the prompt must be QUEUED, NOT
    // delivered within the debounce window (no 2nd recorded prompt yet).
    daemon.run_now("idle").expect("run-now idle #2 (typing)");
    assert!(
        !common::wait_for_file_substr_count(&record, PROMPT_MARKER, 2, Duration::from_millis(900)),
        "with recent user input the reuse prompt must be debounced (not delivered within the window)"
    );

    // Once the pane goes idle (debounce elapses) the queued prompt is delivered.
    assert!(
        common::wait_for_file_substr_count(&record, PROMPT_MARKER, 2, Duration::from_secs(6)),
        "after the debounce window the queued prompt must be delivered into the reused pane"
    );

    // No recent input now → the next reuse fire delivers immediately.
    daemon.run_now("idle").expect("run-now idle #3 (idle)");
    assert!(
        common::wait_for_file_substr_count(&record, PROMPT_MARKER, 3, Duration::from_millis(1800)),
        "with no recent input a reuse fire must deliver immediately"
    );
}
