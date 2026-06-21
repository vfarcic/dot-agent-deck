#![cfg(feature = "e2e")]

//! L2 tests for PRD #170 M1.3 — login-shell PATH parity for daemon-spawned panes.
//!
//! The daemon spawns every pane command through `agent_pty`'s `CommandBuilder`,
//! which makes `portable-pty` resolve a *bare* command against the daemon's own
//! process PATH. When the daemon is launched without the user's login profile,
//! its PATH lacks the dir where `claude`/`opencode` live (`~/.local/bin`), so a
//! bare command fails to spawn. PRD #170's fix captures the login-shell PATH
//! once at daemon startup (`$SHELL -ilc 'printf %s "$PATH"'`) and applies it to
//! the daemon's own environment, so every subsequently-spawned pane inherits it.
//!
//! These tests reproduce the failure without depending on the host's real
//! `~/.local/bin`: a stub binary is placed in a temp dir that is NOT on the
//! daemon's PATH, and the daemon's `$SHELL` is a fake login shell whose
//! `-ilc` output adds that dir to PATH (mirroring how `~/.profile` adds
//! `~/.local/bin`). A bare reference to the stub therefore resolves ONLY if the
//! daemon captured the login-shell PATH. All three of PRD #170's spawn paths are
//! pinned here — the dashboard new-pane (`001`, real TUI), a scheduled-task fire
//! (`002`, headless daemon), and the schedule-authoring helper (`003`, the
//! originally-motivating bug). The authoring-helper path routes through the same
//! daemon spawn primitive and additionally depends on the configurable-command
//! change pinned by `scheduler/manager/002`.
//!
//! GREEN now (PRD #170 merged): the daemon captures the login-shell PATH at
//! startup, so its PATH includes the stub dir, the bare command resolves, the
//! spawn succeeds, and the stub's marker appears. These tests guard against the
//! pre-fix regression where nothing captured the login-shell PATH, so the bare
//! command was not found and the marker never appeared.

mod common;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Paths produced by [`login_path_fixture`].
struct LoginFixture {
    /// Held so the temp tree outlives the test.
    _scratch: tempfile::TempDir,
    /// Bare command name (no path separator, no whitespace) — resolves only via
    /// the login-shell PATH, never via the daemon's inherited PATH.
    stub_name: String,
    /// Absolute path of the marker the stub writes when it runs.
    marker: std::path::PathBuf,
    /// A fake login shell whose `-ilc` output adds the stub dir to PATH.
    fake_shell: std::path::PathBuf,
    /// Working dir for the spawned pane / task.
    work: std::path::PathBuf,
    /// Config file whose `default_command` is the bare stub.
    config: std::path::PathBuf,
}

/// Build a temp tree with:
///   - a stub command (named `stub_name`) in a `stubbin/` dir that writes a
///     marker file then stays alive (so the spawned pane is "running");
///   - a fake login shell that emulates a profile prepending `stubbin/` to PATH
///     and, invoked as `-ilc '<script>'`, runs the script with that enriched
///     PATH — exactly what PRD #170's `$SHELL -ilc 'printf %s "$PATH"'` capture
///     reads;
///   - a `config.toml` whose `default_command` is the bare stub.
///
/// The stub dir is deliberately NOT placed on the process PATH — it reaches the
/// daemon ONLY through the login-shell capture.
fn login_path_fixture(stub_name: &str) -> LoginFixture {
    let scratch = tempfile::tempdir().expect("scratch tempdir");
    let base = scratch.path().to_path_buf();

    let stub_dir = base.join("stubbin");
    std::fs::create_dir_all(&stub_dir).expect("create stub bin dir");
    let work = base.join("work");
    std::fs::create_dir_all(&work).expect("create work dir");

    let marker = base.join("STUB_RAN");

    // The stub: write the marker via a pure-shell redirection (no external
    // binary needed for the create), then stay alive so the card is "running".
    let stub = stub_dir.join(stub_name);
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\n: > \"{marker}\"\nexec sleep 30\n",
            marker = marker.to_string_lossy()
        ),
    )
    .expect("write stub command");

    // The fake login shell: prepend the stub dir to PATH (the one profile effect
    // we emulate), then exec whatever command was requested. Drops any leading
    // flag bundle (`-l`, `-c`, `-lc`, `-ilc`, `-lic`, …) so `$SHELL -ilc '<script>'`
    // works; the capture's `printf` then prints the enriched PATH.
    let fake_shell = base.join("login-shell.sh");
    std::fs::write(
        &fake_shell,
        format!(
            "#!/bin/sh\n\
             export PATH=\"{stub_dir}:$PATH\"\n\
             while [ \"$#\" -gt 0 ]; do\n\
             case \"$1\" in\n\
             -*) shift ;;\n\
             *) break ;;\n\
             esac\n\
             done\n\
             if [ \"$#\" -gt 0 ]; then\n\
             exec /bin/sh -c \"$1\"\n\
             fi\n\
             exec /bin/sh\n",
            stub_dir = stub_dir.to_string_lossy()
        ),
    )
    .expect("write fake login shell");

    // default_command = the bare stub, so the new-pane form opens pre-filled
    // with it (and so PRD #170's authoring helper would too, once it reads
    // default_command).
    let config = base.join("config.toml");
    std::fs::write(&config, format!("default_command = \"{stub_name}\"\n"))
        .expect("write config.toml");

    #[cfg(unix)]
    {
        for p in [&stub, &fake_shell] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fixture script");
        }
    }

    LoginFixture {
        _scratch: scratch,
        stub_name: stub_name.to_string(),
        marker,
        fake_shell,
        work,
        config,
    }
}

/// Scenario: Launch the deck with `default_command` set to a bare stub command
/// that lives ONLY in a dir absent from the inherited PATH, and `$SHELL` pointed
/// at a fake login shell whose `-ilc` output adds that dir to PATH. Open the
/// new-pane form (Ctrl+n → Space confirms the dir) — the Command field is
/// pre-filled with the bare stub — and submit via the `[Submit]` button. Assert
/// the bare command resolves and spawns: the stub writes its on-disk marker.
/// GREEN now (PRD #170 merged): the daemon captures the login-shell PATH at
/// startup, so the bare command resolves and the marker appears; the guarded
/// regression is the pre-fix state where no capture happened and the marker
/// never appeared.
#[spec("lifecycle/login-path/001")]
#[test]
fn login_path_001_new_pane_resolves_login_shell_command() {
    let fx = login_path_fixture("stub-newpane-agent");

    let deck = TuiDeck::builder()
        .with_env("SHELL", fx.fake_shell.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", fx.config.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Open the new-pane form: Ctrl+n → directory picker, Space confirms the
    // current dir → the new-pane form (whose Command field is pre-filled with
    // default_command — the bare stub).
    deck.send_keys(b"\x0e"); // Ctrl+n
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("New Agent"); // the form is up

    // Submit via the [Submit] button (layout-robust: located on the grid, so the
    // PRD #170 agent-command picker addition can't break this drive).
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the new-pane form should render a [Submit] button");
    deck.click(scol, srow);

    assert!(
        common::wait_for_path(&fx.marker, Duration::from_secs(15)),
        "a dashboard new-pane whose command is a bare binary living only in the \
         login-shell PATH must spawn successfully, but its marker never appeared — \
         the daemon should capture the login-shell PATH (`$SHELL -ilc 'printf %s \
         \"$PATH\"'`) at startup so the bare command resolves"
    );
}

/// Scenario: Spawn a headless daemon with `$SHELL` pointed at a fake login shell
/// whose `-ilc` output adds a stub dir (absent from the daemon's PATH) to PATH,
/// and register a scheduled task whose `command` is a bare stub living only in
/// that dir. Fire the task via the `RunNow` control message and assert the bare
/// command resolves and spawns: the stub writes its on-disk marker. GREEN now
/// (PRD #170 merged): the daemon captures the login-shell PATH, so the bare
/// command resolves and the marker appears; the guarded regression is the
/// pre-fix state where, with no capture, the bare command was not found and the
/// marker never appeared.
#[spec("lifecycle/login-path/002")]
#[test]
fn login_path_002_scheduled_fire_resolves_login_shell_command() {
    let fx = login_path_fixture("stub-sched-agent");

    let toml = format!(
        "[[scheduled_tasks]]\n\
         name = \"login-fire\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{work}\"\n\
         command = \"{cmd}\"\n\
         prompt = \"login fire prompt\"\n\
         enabled = true\n",
        work = fx.work.to_string_lossy(),
        cmd = fx.stub_name,
    );

    let shell = fx.fake_shell.to_string_lossy().into_owned();
    let daemon =
        common::spawn_daemon_serve_with_env(Some(&toml), "0", &[("SHELL", shell.as_str())]);

    daemon.run_now("login-fire").expect("run-now login-fire");

    assert!(
        common::wait_for_path(&fx.marker, Duration::from_secs(15)),
        "a scheduled-task fire whose command is a bare binary living only in the \
         login-shell PATH must spawn successfully, but its marker never appeared — \
         the daemon should capture the login-shell PATH (`$SHELL -ilc 'printf %s \
         \"$PATH\"'`) at startup so the bare command resolves"
    );
}

/// Scenario: Launch the deck with `default_command` set to a bare authoring
/// command that lives ONLY in a dir absent from the inherited PATH, and `$SHELL`
/// pointed at a fake login shell whose `-ilc` output adds that dir to PATH, plus a
/// fixture `schedules.toml` holding one task. Open the Scheduled-Tasks manager
/// (`S`), press `e` to edit the auto-selected row — which now reuses the `Ctrl+n`
/// flow (PRD #170 unify): a directory picker (` Select Directory `) → the
/// mode-locked ` Edit Schedule ` form whose Command is pre-filled with the bare
/// `default_command`. Confirm the dir with Space and submit via `[Submit]`.
/// Assert the bare authoring command resolves and spawns under the daemon's
/// login-shell-enriched PATH: the stub writes its on-disk marker. This pins PRD
/// #170's THIRD spawn path (the schedule-authoring helper), the originally-
/// motivating bug — GREEN once the login-shell PATH capture (M1.3), the
/// configurable authoring command (M2.1), and the unified Add/Edit flow are
/// merged.
#[spec("lifecycle/login-path/003")]
#[test]
fn login_path_003_schedule_authoring_resolves_login_shell_command() {
    let fx = login_path_fixture("stub-authoring-agent");

    // One fixture schedule so the manager has a row to edit. The task's OWN run
    // command (`cat`, on the normal PATH) is irrelevant here — the authoring
    // helper's command comes from `default_command` (the bare stub), not the task.
    let sched_dir = tempfile::tempdir().expect("schedules tempdir");
    let sched_path = sched_dir.path().join("schedules.toml");
    std::fs::write(
        &sched_path,
        format!(
            "[[scheduled_tasks]]\n\
             name = \"digest\"\n\
             cron = \"0 9 * * *\"\n\
             working_dir = \"{work}\"\n\
             command = \"cat\"\n\
             prompt = \"digest prompt\"\n\
             enabled = true\n",
            work = fx.work.to_string_lossy(),
        ),
    )
    .expect("write fixture schedules.toml");

    let deck = TuiDeck::builder()
        .with_env("SHELL", fx.fake_shell.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", fx.config.to_string_lossy())
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Open the Scheduled-Tasks manager and edit the auto-selected `digest` row,
    // which reuses the Ctrl+n flow. `NEXT FIRE` is the "dialog is up" signal;
    // ` Select Directory ` is the "dir picker is up" signal; ` Edit Schedule ` is
    // the "mode-locked form is up" signal.
    deck.send_keys(b"S");
    deck.wait_for_string("NEXT FIRE");
    deck.send_keys(b"e"); // edit → opens the dir picker (starting at the row's working_dir)
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space → confirm the dir → mode-locked Edit Schedule form
    deck.wait_for_string("Edit Schedule");

    // The form's Command is pre-filled with the resolved authoring command — the
    // bare `default_command`. Submit via `[Submit]` to spawn the seeded authoring
    // agent running the bare stub through the daemon spawn primitive.
    let (scol, srow) = deck
        .find_in_grid("[Submit]")
        .expect("the mode-locked schedule form must render a [Submit] button");
    deck.click(scol, srow); // submit → spawn the authoring agent

    assert!(
        common::wait_for_path(&fx.marker, Duration::from_secs(15)),
        "the schedule-authoring helper's bare command (a binary living only in the \
         login-shell PATH) must resolve and spawn under the daemon's login-shell-\
         enriched PATH, but its marker never appeared — PRD #170's third spawn path \
         must benefit from the same `$SHELL -ilc 'printf %s \"$PATH\"'` capture so the \
         bare `default_command` resolves"
    );

    drop(sched_dir);
}
