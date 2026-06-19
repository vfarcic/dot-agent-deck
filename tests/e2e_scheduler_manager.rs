#![cfg(feature = "e2e")]

//! L2 tests for PRD #127 M3.3 — the "Scheduled Tasks" management dialog.
//!
//! All L2 (no public L1 dialog render seam — same constraint as
//! `prompt/new-pane/007`): the real TUI is driven via `TuiDeck::send_keys` and
//! observed through the rendered vt100 grid plus the daemon registry / global
//! `schedules.toml`. A fixture global `schedules.toml` is supplied via the
//! `DOT_AGENT_DECK_SCHEDULES` env override (the lazy-spawned daemon inherits the
//! deck's env, so it loads the fixture schedules).
//!
//! ## Pinned keybinding (for the coder)
//! The manager dialog is opened with `S` (Shift+s, mnemonic "Scheduled tasks").
//! It is unbound in dashboard command mode today (`handle_normal_key` matches
//! only j/k, /, ?, r, Enter, g, y, n) so it doesn't collide. In-dialog actions
//! (per the PRD): `a` add, `Enter`/`e` edit, `d` delete (then a confirmation,
//! confirmed with `y`), `r` run-now; the first row is auto-selected.
//!
//! ## Authoring flow (PRD #170 — unified entry points)
//! `a`/`e` no longer open a bespoke pick-agent modal: they reuse the SAME
//! `Ctrl+n` flow — a **directory picker** (` Select Directory `) → the new-pane
//! form MODE-LOCKED to schedule authoring (` New Schedule ` for Add /
//! ` Edit Schedule ` for Edit), which shows only **Dir** + **Command** (no Mode
//! cycler, no Name field). The Command field is pre-filled from the resolved
//! authoring command (the configured `default_command`, or `claude` when that is
//! blank). Confirming (`[Submit]`) spawns the seeded authoring agent running that
//! command IN the picked directory; the scheduled task's own run command is
//! unaffected. Edit additionally starts the picker at the row's `working_dir` and
//! pre-fills the authoring seed with the existing schedule's values.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

const MANAGER_KEY: &[u8] = b"S";

/// Create an isolated scratch dir and write a global `schedules.toml` into it;
/// returns (scratch_dir, schedules_path).
fn scratch_with_schedules(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("scratch tempdir");
    let path = dir.path().join("schedules.toml");
    std::fs::write(&path, body).expect("write fixture schedules.toml");
    (dir, path)
}

/// Scenario: With a fixture global `schedules.toml` containing an enabled task
/// (`digest`) and a disabled task (`paused`), press `S` to open the "Scheduled
/// Tasks" dialog. Assert the dialog lists each task with a status indicator and
/// a next-fire cell — `digest` shows an `idle` status, `paused` shows the
/// `disabled` indicator with a `—` next-fire placeholder. Also assert each
/// action button advertises its keyboard shortcut next to the label
/// (`[Add a]` / `[Edit e]` / `[Delete d]` / `[Run now r]`), mirroring the
/// `[Scheduled Tasks s]` button-bar button.
#[spec("scheduler/manager/001")]
#[test]
fn manager_001_lists_schedules_with_status_and_next_fire() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"digest\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"digest prompt\"\n\
         enabled = true\n\n\
         [[scheduled_tasks]]\n\
         name = \"paused\"\n\
         cron = \"0 10 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"paused prompt\"\n\
         enabled = false\n",
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(MANAGER_KEY);
    // PRD #127 finding #4: the dashboard now carries a `[Scheduled Tasks s]`
    // button-bar button, so the bare "Scheduled Tasks" substring is on-screen
    // BEFORE the dialog opens — waiting for it would snapshot the dashboard. The
    // `NEXT FIRE` column header only renders once the dialog is open with its
    // rows loaded, so it's an unambiguous "dialog is up" signal.
    deck.wait_for_string("NEXT FIRE");

    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("digest") && grid.contains("paused"),
        "manager dialog must list both configured schedules.\nGrid:\n{grid}"
    );
    assert!(
        grid.contains("idle"),
        "the enabled-but-not-live schedule must show an `idle` status.\nGrid:\n{grid}"
    );
    assert!(
        grid.contains("disabled"),
        "the disabled schedule must show a `disabled` status indicator.\nGrid:\n{grid}"
    );
    assert!(
        grid.contains('—'),
        "a disabled schedule's next-fire cell must render the `—` placeholder.\nGrid:\n{grid}"
    );

    // PRD #127: each action button must advertise its keyboard shortcut next to
    // the label — `[Add a]`, `[Edit e]`, `[Delete d]`, `[Run now r]` — mirroring
    // the `[Scheduled Tasks s]` button-bar button, so a keyboard user can tell
    // which key drives each action. Before the fix the buttons rendered `[Add]` /
    // `[Edit]` / `[Delete]` / `[Run now]` with the shortcut field empty
    // (src/ui.rs Button::new(.., "", ..)), so no `<label> <key>` pair appeared.
    for (label, key) in [
        ("Add", "a"),
        ("Edit", "e"),
        ("Delete", "d"),
        ("Run now", "r"),
    ] {
        assert!(
            grid.contains(&format!("{label} {key}")),
            "the `{label}` action button must show its `{key}` shortcut key \
             alongside its label (e.g. `[{label} {key}]`), like the \
             `[Scheduled Tasks s]` button-bar button.\nGrid:\n{grid}"
        );
    }
    drop(scratch);
}

/// Scenario: With a fixture `schedules.toml` containing one task (`digest`) and
/// `default_command` configured to a DISTINCTIVE stub command (`stub-authoring`,
/// not `claude`) shimmed (on PATH) to a recorder agent that posts SessionStart
/// and records its delivered prompt, open the manager and press `e` on the row to
/// edit. Editing now reuses the `Ctrl+n` flow (PRD #170 unify): `e` opens the
/// directory picker (` Select Directory `); confirming the dir with Space opens
/// the new-pane form MODE-LOCKED to schedule (` Edit Schedule `), whose Command
/// is pre-filled from `default_command`; submitting via `[Submit]` spawns the
/// seeded authoring agent. The agent that spawns must be the CONFIGURED command —
/// the `stub-authoring` recorder receives the authoring seed carrying `digest`'s
/// distinctive prompt text (proving both edit pre-fill AND that the confirmed
/// command came from `default_command`) — while a separate `claude` neutralizer
/// shim (kept on PATH so the host's real `claude` is never invoked) records
/// nothing. RED until the new flow exists: today `e` opens the deleted pick-agent
/// modal, so the dir picker never appears and the ` Select Directory ` wait times
/// out.
#[spec("scheduler/manager/002")]
#[test]
fn manager_002_edit_spawns_seeded_authoring_agent_prefilled() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"digest\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"DIGESTPROMPTMARKER\"\n\
         enabled = true\n",
    );

    // Write a recorder shim named `name` into `shim_dir`: it opens the
    // gated-delivery readiness gate (posts SessionStart via the real hook path)
    // then records every delivered line to `record`, so the authoring seed is
    // observable. Used for BOTH the configured stub command and a `claude`
    // neutralizer (the latter so the host's real `claude` never runs and so the
    // hardcoded-`claude` regression is observable).
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let shim_dir = scratch.path().join("shim");
    std::fs::create_dir_all(&shim_dir).expect("create shim dir");
    let write_recorder = |name: &str, record: &std::path::Path| {
        let path = shim_dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n\
                 printf '%s' '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"authoring\"}}' \
                 | \"{bin}\" hook claude-code >/dev/null 2>&1\n\
                 while IFS= read -r l; do printf '%s\\n' \"$l\" >> \"{rec}\"; done\n",
                rec = record.to_string_lossy()
            ),
        )
        .unwrap_or_else(|e| panic!("write {name} shim: {e}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .unwrap_or_else(|e| panic!("chmod {name} shim: {e}"));
        }
    };

    // The CONFIGURED authoring command is a distinctive stub — its recorder is
    // the GREEN signal. A `claude` neutralizer recorder shields the test from the
    // host's real `claude` AND catches any regression where the authoring command
    // falls back to `claude` instead of honoring the configured `default_command`.
    let authoring_record = scratch.path().join("authoring-record.log");
    let claude_record = scratch.path().join("claude-record.log");
    write_recorder("stub-authoring", &authoring_record);
    write_recorder("claude", &claude_record);

    // `default_command` = the distinctive stub, supplied via a config file the
    // deck reads (DOT_AGENT_DECK_CONFIG). PRD #170 M2.1 (merged) makes the authoring
    // helper resolve its command from this instead of the hardcoded `claude`.
    let config_path = scratch.path().join("config.toml");
    std::fs::write(&config_path, "default_command = \"stub-authoring\"\n")
        .expect("write config.toml");

    let path_env = format!(
        "{}:{}",
        shim_dir.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_env("PATH", path_env)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(MANAGER_KEY);
    deck.wait_for_string("Scheduled Tasks");
    deck.send_keys(b"e"); // edit the auto-selected `digest` row → opens the dir picker

    // PRD #170 unify: Edit now reuses the Ctrl+n flow. `e` opens the directory
    // picker (starting at the row's working_dir); confirm the dir with Space to
    // reach the mode-locked ` Edit Schedule ` form, then submit via `[Submit]`.
    // RED today: `e` opens the deleted pick-agent modal, so the dir picker's
    // ` Select Directory ` chrome never renders and this wait times out.
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm the (row) dir → locked schedule form
    deck.wait_for_string("Edit Schedule"); // the mode-locked Edit form is up
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the mode-locked schedule form must render a [Submit] button");
    deck.click(scol, srow); // submit → spawn the seeded authoring agent

    // Submitting must spawn the CONFIGURED authoring command (not `claude`) and
    // pre-fill it: the stub's recorder receives the seed carrying digest's current
    // prompt value.
    assert!(
        common::wait_for_file_substr_count(
            &authoring_record,
            "DIGESTPROMPTMARKER",
            1,
            Duration::from_secs(15),
        ),
        "editing a schedule must open the dir picker → mode-locked Edit Schedule form, then on \
         submit spawn the seeded authoring agent running the CONFIGURED `default_command` \
         (`stub-authoring`), pre-filled with the row's current values — the configured command's \
         recorder never received digest's prompt"
    );
    // The confirmed authoring command must NOT be `claude`: its neutralizer
    // recorder must be empty (checked only after the positive assert passes, so
    // this is a clean point-in-time read, not a race).
    assert_eq!(
        common::count_file_substr(&claude_record, "DIGESTPROMPTMARKER"),
        0,
        "the confirmed authoring command must come from `default_command`, not `claude` \
         — but the `claude` shim received the authoring seed"
    );
    drop(scratch);
}

/// Scenario: With a fixture task (`killme`) whose fire spawns a live agent,
/// first fire it via the RunNow control message so an open tab exists, then open
/// the manager, press `d` and confirm with `y`. Assert the schedule DEFINITION
/// is removed from the global `schedules.toml` (the reloaded list no longer has
/// it) AND the already-open tab/agent for that schedule survives the delete.
#[spec("scheduler/manager/003")]
#[test]
fn manager_003_delete_removes_definition_but_keeps_open_tab() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"killme\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"killme prompt\"\n\
         enabled = true\n",
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Open a tab for the schedule by firing it (existing RunNow socket path).
    common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: "killme".to_string(),
        },
    )
    .expect("RunNow killme");
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "killme",
            true,
            Duration::from_secs(10),
        ),
        "firing the schedule must open a live tab/agent for it"
    );

    // Delete the definition via the manager.
    deck.send_keys(MANAGER_KEY);
    deck.wait_for_string("Scheduled Tasks");
    deck.send_keys(b"d"); // delete the auto-selected row → confirmation
    deck.send_keys(b"y"); // confirm

    // Half 1: the definition is gone from the global config.
    assert!(
        common::wait_for_schedule_absent_from_file(&sched_path, "killme", Duration::from_secs(10)),
        "confirming delete must remove the schedule DEFINITION from schedules.toml"
    );
    // Half 2: the already-open tab/agent survives (delete is definition-only).
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "killme",
            true,
            Duration::from_secs(2),
        ),
        "deleting a schedule must NOT close an already-open tab for it"
    );
    drop(scratch);
}

/// Scenario: With a fixture task whose name is LONG, open the manager and press
/// `d` to arm the delete confirmation. The confirmation renders on two fixed
/// natural lines — the name on its own line (`Delete schedule '…'?`) and the
/// fixed `definition only — open tab kept. (y/n)` trailer on the next. Assert the
/// trailing `(y/n)` prompt is CONTAINED WITHIN the modal: under PRD #144 the
/// modal is content-sized, so it grows in WIDTH to contain the long name line
/// (clamped to ≤90% of the terminal) and the second line's `(y/n)` tail is never
/// clipped off the right border. (Supersedes the PRD #127 band-aid that wrapped
/// the message to grow the modal in HEIGHT inside a fixed 72-col modal.)
#[spec("scheduler/manager/005")]
#[test]
fn manager_005_delete_confirm_contained_within_modal() {
    // A name long enough that the single-line form of the confirmation would
    // overflow a fixed-width modal — exercising the PRD #144 content-driven WIDTH
    // growth that keeps both natural lines (the name line; the `… (y/n)` trailer)
    // un-clipped instead of spilling the tail past the border.
    const LONG_NAME: &str = "extremely-long-scheduled-task-name-that-overflows-the-modal";

    let (scratch, sched_path) = scratch_with_schedules(&format!(
        "[[scheduled_tasks]]\n\
         name = \"{LONG_NAME}\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"overflow prompt\"\n\
         enabled = true\n"
    ));

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(MANAGER_KEY);
    deck.wait_for_string("Scheduled Tasks");
    deck.send_keys(b"d"); // arm the delete confirmation for the auto-selected row
    deck.wait_for_string("Delete schedule"); // the (left-aligned) prefix is always visible

    let grid = deck.snapshot_grid();
    // The full confirmation must stay inside the modal: its trailing `(y/n)`
    // prompt — the only `(y/n)` in the whole app — must render. The confirmation
    // sits on two fixed natural lines and the modal grows in WIDTH to contain the
    // long name line (PRD #144 content-sizing), so the second line's `(y/n)` tail
    // is never clipped off the right border.
    assert!(
        grid.contains("(y/n)"),
        "the delete confirmation overflowed the modal: the long schedule name pushed \
         the message past the modal's inner width, clipping the trailing `(y/n)` prompt \
         off the right edge. The confirmation must be contained within the modal border \
         (wrapped, name on its own line).\nGrid:\n{grid}"
    );
    drop(scratch);
}

/// Scenario: With a fixture task (`firetask`), open the manager and press `r`
/// on the row to trigger an immediate run-now fire. Assert the fire happened —
/// the task spawns its tab/agent (registered with the task's display name).
#[spec("scheduler/manager/004")]
#[test]
fn manager_004_run_now_fires_selected_task() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"firetask\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"firetask prompt\"\n\
         enabled = true\n",
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // No agent for the task yet.
    assert!(
        !common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "firetask",
            true,
            Duration::from_millis(300),
        ),
        "precondition: the task has not fired yet"
    );

    deck.send_keys(MANAGER_KEY);
    deck.wait_for_string("Scheduled Tasks");
    deck.send_keys(b"r"); // run-now the auto-selected row

    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "firetask",
            true,
            Duration::from_secs(10),
        ),
        "pressing `r` in the manager must run-now the selected task (it never fired)"
    );
    drop(scratch);
}

/// Scenario: With a fixture global `schedules.toml` holding TWO enabled tasks
/// (`alpha` then `bravo`), press `S` to open the manager. `alpha` (the first
/// row) is auto-selected, so the `▶` selection marker sits on it. Left-click
/// the `bravo` row — which is NOT currently selected — and assert the selection
/// marker moves to `bravo` (the rendered `▶ bravo` indicator appears and
/// `▶ alpha` is gone), proving a row click hit-tests and re-selects. Before the
/// fix, clicking a row was a no-op (selection only moved via the keyboard j/k),
/// so the marker stayed on `alpha`.
#[spec("scheduler/manager/006")]
#[test]
fn manager_006_click_row_moves_selection() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"alpha\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"alpha prompt\"\n\
         enabled = true\n\n\
         [[scheduled_tasks]]\n\
         name = \"bravo\"\n\
         cron = \"0 10 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"bravo prompt\"\n\
         enabled = true\n",
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(MANAGER_KEY);
    // `NEXT FIRE` only renders once the dialog is open with its rows loaded.
    deck.wait_for_string("NEXT FIRE");

    // Precondition: the first row (`alpha`) is auto-selected — the `▶` marker
    // (U+25B6 + space) sits on it, and NOT on `bravo`.
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("\u{25b6} alpha"),
        "precondition: the first row (`alpha`) must be auto-selected, marked \
         with `▶`.\nGrid:\n{grid}"
    );
    assert!(
        !grid.contains("\u{25b6} bravo"),
        "precondition: the second row (`bravo`) must NOT start selected.\nGrid:\n{grid}"
    );

    // Click the (currently unselected) `bravo` row at its on-screen position.
    let (col, row) = deck
        .find_in_grid("bravo")
        .expect("the manager list must render the `bravo` row");
    deck.click(col, row);

    // The selection marker must move to the clicked row: `▶ bravo` now renders.
    // Before the fix, clicking a row was a no-op, so this never appeared and the
    // wait timed out with the marker still on `alpha`.
    deck.wait_for_string("\u{25b6} bravo");

    // And the selection has left `alpha` (exactly one row is selected at a time).
    let grid = deck.snapshot_grid();
    assert!(
        !grid.contains("\u{25b6} alpha"),
        "after clicking `bravo`, the selection marker must leave `alpha`.\nGrid:\n{grid}"
    );
    drop(scratch);
}

/// Scenario: With a fixture `schedules.toml` holding one enabled task whose name
/// is LONGER than the legacy fixed-width name cell, open the "Scheduled Tasks"
/// manager at a roomy (200-col) terminal and again at a windowed (80-col)
/// terminal. Assert the task's FULL name renders un-clipped on the grid at BOTH
/// widths — proving the dialog auto-sizes to its content (PRD #144 shared modal
/// sizing helper, clamped within the windowed terminal) instead of truncating
/// the field to a fixed 72-col modal. Before the fix the modal was hard-capped at
/// 72 columns and the name was truncated to 21 chars (`truncate_cell`), so the
/// full name never appeared at either width.
#[spec("scheduler/manager/007")]
#[test]
fn manager_007_dialog_content_sized_unclipped_at_both_widths() {
    // Longer than the legacy 21-char name cell, yet short enough to fit a
    // content-sized modal even at the 80-col windowed floor.
    const LONG_NAME: &str = "nightly-backup-and-report";

    let (scratch, sched_path) = scratch_with_schedules(&format!(
        "[[scheduled_tasks]]\n\
         name = \"{LONG_NAME}\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"backup prompt\"\n\
         enabled = true\n"
    ));

    // Open the manager at a given terminal size and return the rendered grid.
    fn manager_grid(cols: u16, rows: u16, sched_path: &std::path::Path) -> String {
        let deck = TuiDeck::builder()
            .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
            .with_pty_size(cols, rows)
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active sessions");
        deck.send_keys(MANAGER_KEY);
        // `NEXT FIRE` only renders once the dialog is open with its rows loaded —
        // an unambiguous "dialog is up" signal (also proves the column labels
        // render un-clipped).
        deck.wait_for_string("NEXT FIRE");
        deck.snapshot_grid()
    }

    // Roomy width: the content-sized modal grows to show the full name.
    let roomy = manager_grid(200, 40, &sched_path);
    assert!(
        roomy.contains(LONG_NAME),
        "at a roomy 200-col width the manager dialog must auto-size to its \
         content and render the full schedule name `{LONG_NAME}` un-clipped \
         (today the modal is capped at 72 cols and the name is truncated to 21 \
         chars).\nGrid:\n{roomy}"
    );

    // Windowed width: the modal clamps within the terminal but still renders the
    // full name un-clipped (no field clipped off the modal border).
    let windowed = manager_grid(80, 30, &sched_path);
    assert!(
        windowed.contains(LONG_NAME),
        "at a windowed 80-col width the manager dialog must still render the \
         full schedule name `{LONG_NAME}` un-clipped within the modal.\nGrid:\n{windowed}"
    );

    drop(scratch);
}

/// Write a recorder shim named `name` into `shim_dir`: it records its working
/// directory (`pwd`) — the dir the agent was SPAWNED in — then opens the
/// gated-delivery readiness gate (posts SessionStart via the real hook path) and
/// appends every delivered line to `record`, so BOTH the spawn cwd AND the
/// authoring seed a spawned agent receives are observable on disk. Distinct-name
/// shims let a test tell WHICH agent command actually spawned; the recorded
/// `pwd` lets a test assert the agent spawned in the PICKED directory. Mirrors
/// the inline recorder in `manager_002`.
fn write_recorder_shim(shim_dir: &std::path::Path, name: &str, record: &std::path::Path) {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let path = shim_dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             pwd >> \"{rec}\"\n\
             printf '%s' '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"authoring\"}}' \
             | \"{bin}\" hook claude-code >/dev/null 2>&1\n\
             while IFS= read -r l; do printf '%s\\n' \"$l\" >> \"{rec}\"; done\n",
            rec = record.to_string_lossy()
        ),
    )
    .unwrap_or_else(|e| panic!("write {name} shim: {e}"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod {name} shim: {e}"));
    }
}

/// Scenario: With a fixture task (`placeholder`, so the manager opens) and
/// `default_command` EMPTY/unset (the unconfigured-user case), put a `claude`
/// recorder shim on PATH, open the manager and press `a` to ADD. Adding reuses
/// the `Ctrl+n` flow: `a` opens the directory picker (` Select Directory `);
/// confirming the dir with Space opens the mode-locked ` New Schedule ` form
/// whose Command is PRE-FILLED from the resolved authoring command — and with a
/// blank `default_command` that resolves to `claude` (`AGENT_COMMAND_PRESETS[0]`).
/// Submit via `[Submit]` and assert the spawned authoring agent runs `claude`
/// (its recorder receives the base authoring seed — `throwaway authoring
/// session`) — the R1 fallback: a blank `default_command` must resolve to
/// `claude`, NOT spawn a bare `$SHELL` that cannot act on the seed. RED until the
/// new flow exists: today `a` opens the deleted pick-agent modal, so the dir
/// picker never appears and the ` Select Directory ` wait times out.
#[spec("scheduler/manager/010")]
#[test]
fn manager_010_blank_default_command_falls_back_to_claude() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"placeholder\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"placeholder prompt\"\n\
         enabled = true\n",
    );

    let shim_dir = scratch.path().join("shim");
    std::fs::create_dir_all(&shim_dir).expect("create shim dir");
    // The R1 fallback target: a blank `default_command` must resolve to `claude`.
    let claude_record = scratch.path().join("claude-record.log");
    write_recorder_shim(&shim_dir, "claude", &claude_record);

    // `default_command = ""` — the unconfigured-user case (config.rs defaults it
    // to an empty String). Written explicitly so the deck never inherits the
    // host's real config.
    let config_path = scratch.path().join("config.toml");
    std::fs::write(&config_path, "default_command = \"\"\n").expect("write config.toml");

    let path_env = format!(
        "{}:{}",
        shim_dir.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_env("PATH", path_env)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(MANAGER_KEY);
    deck.wait_for_string("Scheduled Tasks");
    deck.send_keys(b"a"); // ADD → opens the dir picker (blank-context add)

    // PRD #170 unify: `a` opens the dir picker; confirm the dir with Space to
    // reach the mode-locked ` New Schedule ` form (Command pre-filled with the
    // resolved authoring command), then submit via `[Submit]`.
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm the dir → locked schedule form
    deck.wait_for_string("New Schedule"); // the mode-locked Add form is up
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the mode-locked schedule form must render a [Submit] button");
    deck.click(scol, srow); // submit → spawn the seeded authoring agent

    // R1 fallback: a blank `default_command` must resolve to `claude`
    // (`AGENT_COMMAND_PRESETS[0]`) so a real conversational agent runs — NOT a
    // bare `$SHELL`. The `claude` recorder receiving the base authoring seed
    // (`throwaway authoring session`) proves the fallback. (Add has no row, so the
    // seed carries no row marker — the base-seed substring is the green signal.)
    assert!(
        common::wait_for_file_substr_count(
            &claude_record,
            "throwaway authoring session",
            1,
            Duration::from_secs(15),
        ),
        "an unset/blank `default_command` must fall back to `claude` (not spawn a bare \
         `$SHELL`): the authoring agent must run `claude` and deliver the base authoring \
         seed — the `claude` recorder never received it"
    );
    drop(scratch);
}

// ---------------------------------------------------------------------------
// scheduler/form — the manager Add/Edit now reuse the Ctrl+n dir-picker +
// new-pane form mode-locked to schedule (PRD #170 unify).
// ---------------------------------------------------------------------------

/// Scenario: With `default_command` configured to a distinctive `stub-add-authoring`
/// recorder shim (which records its spawn `pwd` then the delivered seed) and a
/// `claude` neutralizer on PATH, open the manager and press `a` to ADD. Adding
/// reuses the `Ctrl+n` flow: `a` opens the directory picker (` Select Directory `);
/// confirming the current dir with Space opens the mode-locked ` New Schedule `
/// form whose Command is pre-filled from `default_command`; submitting via
/// `[Submit]` spawns the seeded authoring agent. Assert the agent spawns (its
/// recorder receives the base authoring seed — `throwaway authoring session`) and
/// that it spawns IN the picked directory: its recorded `pwd` carries the deck's
/// working-dir basename (the dir the picker confirmed). The `claude` neutralizer
/// stays empty (the form's configured command spawned, not `claude`). RED until
/// the new flow exists: today `a` opens the deleted pick-agent modal, so the dir
/// picker never appears and the ` Select Directory ` wait times out.
#[spec("scheduler/form/002")]
#[test]
fn form_002_add_spawns_authoring_agent_in_picked_dir() {
    // One benign row so the manager opens (Add itself uses no row).
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"placeholder\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"placeholder prompt\"\n\
         enabled = true\n",
    );

    let shim_dir = scratch.path().join("shim");
    std::fs::create_dir_all(&shim_dir).expect("create shim dir");
    let authoring_record = scratch.path().join("authoring-record.log");
    let claude_record = scratch.path().join("claude-record.log");
    write_recorder_shim(&shim_dir, "stub-add-authoring", &authoring_record);
    write_recorder_shim(&shim_dir, "claude", &claude_record);

    let config_path = scratch.path().join("config.toml");
    std::fs::write(&config_path, "default_command = \"stub-add-authoring\"\n")
        .expect("write config.toml");

    let path_env = format!(
        "{}:{}",
        shim_dir.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_env("PATH", path_env)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // The picker for Add opens at the deck's cwd; confirming it with Space makes
    // the picked dir = the deck's working dir. Its basename is the spawn-cwd marker.
    let picked_basename = deck
        .workdir()
        .file_name()
        .expect("deck workdir has a basename")
        .to_string_lossy()
        .into_owned();

    deck.send_keys(MANAGER_KEY);
    deck.wait_for_string("Scheduled Tasks");
    deck.send_keys(b"a"); // ADD → opens the dir picker

    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm the current dir → locked schedule form
    deck.wait_for_string("New Schedule"); // the mode-locked Add form is up
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the mode-locked schedule form must render a [Submit] button");
    deck.click(scol, srow); // submit → spawn the seeded authoring agent

    // The configured command spawned the authoring agent (base seed delivered).
    assert!(
        common::wait_for_file_substr_count(
            &authoring_record,
            "throwaway authoring session",
            1,
            Duration::from_secs(15),
        ),
        "adding a schedule must open the dir picker → mode-locked New Schedule form, then on \
         submit spawn the seeded authoring agent running the configured `default_command` \
         (`stub-add-authoring`) — its recorder never received the base authoring seed"
    );
    // And it spawned IN the picked directory: the recorded spawn `pwd` carries the
    // picked dir's basename (the dir the picker confirmed = the deck's cwd).
    assert!(
        common::count_file_substr(&authoring_record, &picked_basename) >= 1,
        "the authoring agent must spawn IN the picked directory — its recorded `pwd` must \
         carry the picked dir's basename `{picked_basename}`, but it did not"
    );
    // The `claude` neutralizer must stay empty (the form's configured command won).
    assert_eq!(
        common::count_file_substr(&claude_record, "throwaway authoring session"),
        0,
        "the form's configured command must spawn, not `claude` — but the `claude` shim \
         received the authoring seed"
    );
    drop(scratch);
}

/// Scenario: With `default_command` configured to a distinctive `stub-edit-authoring`
/// recorder shim (records its spawn `pwd` then the delivered seed) plus a `claude`
/// neutralizer on PATH, and a fixture task (`digest`) whose `working_dir` is a
/// distinctively-named existing directory (`.../EDITWORKDIR`) and whose prompt is
/// `EDITPROMPTMARKER`, open the manager and press `e` to EDIT. Editing reuses the
/// `Ctrl+n` flow but the dir picker STARTS at the row's `working_dir`: confirming
/// it with Space (without navigating) opens the mode-locked ` Edit Schedule `
/// form; submitting via `[Submit]` spawns the seeded authoring agent. Assert the
/// authoring seed is PRE-FILLED with the existing schedule's values (the recorder
/// receives `EDITPROMPTMARKER`) AND that the agent spawns IN the row's
/// `working_dir` (its recorded `pwd` carries `EDITWORKDIR`) — proving both the
/// edit pre-fill and that the picker started at, and pre-seeded, the row's dir.
/// The `claude` neutralizer stays empty. RED until the new flow exists: today `e`
/// opens the deleted pick-agent modal, so the dir picker never appears and the
/// ` Select Directory ` wait times out.
#[spec("scheduler/form/003")]
#[test]
fn form_003_edit_prefills_seed_and_spawns_in_row_working_dir() {
    // A distinctively-named existing directory for the row's working_dir, so the
    // spawn-cwd marker (`EDITWORKDIR`) is a known literal regardless of the
    // tempdir prefix. Held alive for the whole test.
    let row_work_parent = tempfile::tempdir().expect("row working_dir parent");
    let row_work = row_work_parent.path().join("EDITWORKDIR");
    std::fs::create_dir(&row_work).expect("create row working_dir");

    let (scratch, sched_path) = scratch_with_schedules(&format!(
        "[[scheduled_tasks]]\n\
         name = \"digest\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"{work}\"\n\
         command = \"cat\"\n\
         prompt = \"EDITPROMPTMARKER\"\n\
         enabled = true\n",
        work = row_work.to_string_lossy(),
    ));

    let shim_dir = scratch.path().join("shim");
    std::fs::create_dir_all(&shim_dir).expect("create shim dir");
    let authoring_record = scratch.path().join("authoring-record.log");
    let claude_record = scratch.path().join("claude-record.log");
    write_recorder_shim(&shim_dir, "stub-edit-authoring", &authoring_record);
    write_recorder_shim(&shim_dir, "claude", &claude_record);

    let config_path = scratch.path().join("config.toml");
    std::fs::write(&config_path, "default_command = \"stub-edit-authoring\"\n")
        .expect("write config.toml");

    let path_env = format!(
        "{}:{}",
        shim_dir.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", config_path.to_string_lossy())
        .with_env("PATH", path_env)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    deck.send_keys(MANAGER_KEY);
    deck.wait_for_string("Scheduled Tasks");
    deck.send_keys(b"e"); // EDIT the auto-selected `digest` row → opens the dir picker

    // The picker for Edit STARTS at the row's working_dir; confirm it with Space
    // (no navigation) so the picked dir = the row's working_dir.
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm the (row) dir → locked schedule form
    deck.wait_for_string("Edit Schedule"); // the mode-locked Edit form is up
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the mode-locked schedule form must render a [Submit] button");
    deck.click(scol, srow); // submit → spawn the seeded authoring agent

    // The authoring seed is pre-filled with the existing schedule's values: the
    // recorder receives the row's distinctive prompt text.
    assert!(
        common::wait_for_file_substr_count(
            &authoring_record,
            "EDITPROMPTMARKER",
            1,
            Duration::from_secs(15),
        ),
        "editing a schedule must pre-fill the authoring seed with the existing schedule's \
         values — the recorder never received the row's `EDITPROMPTMARKER` prompt"
    );
    // And the agent spawns IN the row's working_dir (the picker started there and
    // that dir is pre-seeded as the spawn cwd): the recorded `pwd` carries the
    // distinctive `EDITWORKDIR` basename. Had the picker opened at the deck's cwd
    // instead, the spawn cwd would not contain `EDITWORKDIR`.
    assert!(
        common::count_file_substr(&authoring_record, "EDITWORKDIR") >= 1,
        "the Edit dir picker must start at the row's working_dir and pre-seed it as the \
         spawn cwd — the authoring agent's recorded `pwd` must carry `EDITWORKDIR`, but it \
         did not"
    );
    // The `claude` neutralizer must stay empty (the form's configured command won).
    assert_eq!(
        common::count_file_substr(&claude_record, "EDITPROMPTMARKER"),
        0,
        "the form's configured command must spawn, not `claude` — but the `claude` shim \
         received the authoring seed"
    );
    drop(scratch);
    drop(row_work_parent);
}
