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
//! ## Authoring flow (PRD #170)
//! `a`/`e` no longer spawn the authoring agent immediately: they first open the
//! pick-agent modal (`UiMode::ScheduleAgentPick`), which surfaces the
//! agent-command presets (`claude` / `opencode`), defaulting to the resolved
//! authoring command (the configured `default_command`, or `claude` when that is
//! blank). Confirming the modal spawns the seeded authoring agent running the
//! chosen command; the scheduled task's own run command is unaffected.

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
    // which key drives each action. RED today: the buttons render `[Add]` /
    // `[Edit]` / `[Delete]` / `[Run now]` with the shortcut field empty
    // (src/ui.rs Button::new(.., "", ..)), so no `<label> <key>` pair appears.
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
/// edit. Editing now FIRST opens the pick-agent modal (PRD #170 round 2, Option
/// B): assert the modal surfaces the agent-command presets (the `opencode` chip),
/// then confirm the DEFAULT selection with Enter. The seeded authoring agent that
/// then gets spawned must be the CONFIGURED command — the `stub-authoring`
/// recorder receives the authoring seed carrying `digest`'s distinctive prompt
/// text (proving both edit pre-fill AND that the confirmed command came from
/// `default_command`) — while a separate `claude` neutralizer shim (kept on PATH
/// so the host's real `claude` is never invoked) records nothing. RED until the
/// pick-agent modal exists: today `e` spawns the authoring agent immediately with
/// no modal, so the `opencode` preset chip never renders and the modal wait times
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
    // deck reads (DOT_AGENT_DECK_CONFIG). Once PRD #170 M2.1 lands, the authoring
    // helper resolves its command from this instead of the hardcoded `claude`.
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
    deck.send_keys(b"e"); // edit the auto-selected `digest` row → opens the pick-agent modal

    // PRD #170 round 2 (Option B): Edit now routes through the ScheduleAgentPick
    // modal BEFORE spawning. The modal surfaces the agent-command presets
    // (`claude` / `opencode`); wait for the `opencode` preset chip to confirm the
    // modal is up, then confirm the DEFAULT selection (the resolved
    // `default_command`) with Enter. RED until the modal exists: today `e` spawns
    // the authoring agent immediately, so `opencode` never renders and this wait
    // times out.
    deck.wait_for_string("opencode");
    deck.send_keys(b"\r"); // confirm the default selection → spawn the authoring agent

    // Confirming the modal must spawn the CONFIGURED authoring command (not
    // `claude`) and pre-fill it: the stub's recorder receives the seed carrying
    // digest's current prompt value.
    assert!(
        common::wait_for_file_substr_count(
            &authoring_record,
            "DIGESTPROMPTMARKER",
            1,
            Duration::from_secs(15),
        ),
        "editing a schedule must open the pick-agent modal, then on confirm spawn the seeded \
         authoring agent running the CONFIGURED `default_command` (`stub-authoring`), pre-filled \
         with the row's current values — the configured command's recorder never received \
         digest's prompt"
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
/// `▶ alpha` is gone), proving a row click hit-tests and re-selects. RED today:
/// clicking a row is a no-op (selection only moves via the keyboard j/k), so the
/// marker stays on `alpha`.
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
    // RED today — clicking a row is a no-op, so this never appears and the wait
    // times out with the marker still on `alpha`.
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
/// the field to a fixed 72-col modal. RED today: the modal is hard-capped at 72
/// columns and the name is truncated to 21 chars (`truncate_cell`), so the full
/// name never appears at either width.
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

/// Write a recorder shim named `name` into `shim_dir`: it opens the
/// gated-delivery readiness gate (posts SessionStart via the real hook path)
/// then appends every delivered line to `record`, so the authoring seed a
/// spawned agent receives is observable on disk. Distinct-name shims let a test
/// tell WHICH agent command actually spawned. Mirrors the inline recorder in
/// `manager_002`.
fn write_recorder_shim(shim_dir: &std::path::Path, name: &str, record: &std::path::Path) {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
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
}

/// Scenario: With a fixture task (`digest`) and `default_command = "claude"`
/// (so the pick-agent modal opens with `claude` as the default selection), put
/// distinct-name recorder shims for BOTH `claude` and `opencode` on PATH, open
/// the manager and press `e` to edit. When the pick-agent modal appears, select
/// the NON-default `opencode` preset (`l` moves the highlight to the next preset)
/// and confirm with Enter. Assert the seeded authoring agent that spawns is the
/// CHOSEN `opencode` (its recorder receives digest's authoring seed) and that the
/// default `claude` is NOT spawned (its recorder stays empty) — proving a
/// non-default preset selection is honored. RED until the modal + selection
/// wiring exist: today `e` spawns `claude` (the default) immediately with no
/// modal, so `opencode` never renders and the modal wait times out.
#[spec("scheduler/manager/009")]
#[test]
fn manager_009_pick_non_default_preset_is_honored() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"digest\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"DIGESTPROMPTMARKER\"\n\
         enabled = true\n",
    );

    let shim_dir = scratch.path().join("shim");
    std::fs::create_dir_all(&shim_dir).expect("create shim dir");
    let claude_record = scratch.path().join("claude-record.log");
    let opencode_record = scratch.path().join("opencode-record.log");
    write_recorder_shim(&shim_dir, "claude", &claude_record);
    write_recorder_shim(&shim_dir, "opencode", &opencode_record);

    // `default_command = "claude"` → the modal opens defaulting to `claude`; the
    // test then steers it to the non-default `opencode`.
    let config_path = scratch.path().join("config.toml");
    std::fs::write(&config_path, "default_command = \"claude\"\n").expect("write config.toml");

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
    deck.send_keys(b"e"); // edit the auto-selected `digest` row → opens the pick-agent modal

    // The pick-agent modal must appear with the presets; `opencode` (the
    // non-default preset chip) is the unambiguous "modal is up" signal.
    deck.wait_for_string("opencode");
    // Steer the selection to the NON-default preset, then confirm: `l` moves the
    // preset highlight right (claude → opencode), Enter confirms the chosen
    // command and spawns the authoring agent.
    deck.send_keys(b"l"); // claude → opencode
    deck.send_keys(b"\r"); // confirm the chosen `opencode`

    // GREEN signal: the CHOSEN `opencode` spawned — its recorder receives the
    // authoring seed carrying digest's prompt value.
    assert!(
        common::wait_for_file_substr_count(
            &opencode_record,
            "DIGESTPROMPTMARKER",
            1,
            Duration::from_secs(15),
        ),
        "selecting the non-default `opencode` preset and confirming must spawn the authoring \
         agent running `opencode` — its recorder never received digest's authoring seed"
    );
    // The default `claude` must NOT have spawned (checked only after the positive
    // assert passes, so it is a clean point-in-time read, not a race).
    assert_eq!(
        common::count_file_substr(&claude_record, "DIGESTPROMPTMARKER"),
        0,
        "picking `opencode` must override the `claude` default — but the `claude` shim \
         received the authoring seed"
    );
    drop(scratch);
}

/// Scenario: With a fixture task (`digest`) and `default_command` EMPTY/unset
/// (the unconfigured-user case), put a `claude` recorder shim on PATH, open the
/// manager and press `e` to edit. When the pick-agent modal appears, confirm the
/// DEFAULT selection with Enter. Assert the authoring agent that spawns runs
/// `claude` (`AGENT_COMMAND_PRESETS[0]`, caught by the `claude` recorder) — the
/// R1 fallback: a blank `default_command` must resolve to `claude`, NOT spawn a
/// bare `$SHELL` that cannot act on the seed. RED today: a blank `default_command`
/// spawns a bare login shell (no modal, no fallback), so the `claude` recorder is
/// never written and the modal wait times out.
#[spec("scheduler/manager/010")]
#[test]
fn manager_010_blank_default_command_falls_back_to_claude() {
    let (scratch, sched_path) = scratch_with_schedules(
        "[[scheduled_tasks]]\n\
         name = \"digest\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"DIGESTPROMPTMARKER\"\n\
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
    deck.send_keys(b"e"); // edit the auto-selected `digest` row → opens the pick-agent modal

    // With a blank `default_command` the modal still renders the presets (its
    // default selection is the `claude` fallback); `opencode` is the "modal is up"
    // signal. Confirm the default with Enter.
    deck.wait_for_string("opencode");
    deck.send_keys(b"\r"); // confirm the default selection → spawn the authoring agent

    // R1 fallback: a blank `default_command` must resolve to `claude`
    // (`AGENT_COMMAND_PRESETS[0]`) so a real conversational agent runs — NOT a
    // bare `$SHELL`. The `claude` recorder receiving the seed proves the fallback.
    assert!(
        common::wait_for_file_substr_count(
            &claude_record,
            "DIGESTPROMPTMARKER",
            1,
            Duration::from_secs(15),
        ),
        "an unset/blank `default_command` must fall back to `claude` (not spawn a bare \
         `$SHELL`): the authoring agent must run `claude` and deliver digest's seed — \
         the `claude` recorder never received it"
    );
    drop(scratch);
}
