#![cfg(feature = "e2e")]

//! L2 tests for PRD #170 M1.3 — login-shell PATH parity for daemon-spawned panes.
//!
//! The daemon spawns every pane command through `agent_pty`'s `CommandBuilder`,
//! which makes `portable-pty` resolve a *bare* command against the daemon's own
//! process PATH. When the daemon is launched without the user's login profile,
//! its PATH lacks the dir where `claude`/`opencode` live (`~/.local/bin`), so a
//! bare command fails to spawn. PRD #170's fix captures the login-shell PATH
//! once at daemon startup (`$SHELL -lc 'printf %s "$PATH"'`) and applies it to
//! the daemon's own environment, so every subsequently-spawned pane inherits it.
//!
//! These tests reproduce the failure without depending on the host's real
//! `~/.local/bin`: a stub binary is placed in a temp dir that is NOT on the
//! daemon's PATH, and the daemon's `$SHELL` is a fake login shell whose
//! `-lc` output adds that dir to PATH (mirroring how `~/.profile` adds
//! `~/.local/bin`). A bare reference to the stub therefore resolves ONLY if the
//! daemon captured the login-shell PATH. Two of PRD #170's three spawn paths are
//! pinned here — the dashboard new-pane (`001`, real TUI) and a scheduled-task
//! fire (`002`, headless daemon). The third path (the schedule-authoring helper)
//! routes through the same daemon spawn primitive and additionally depends on the
//! configurable-command change pinned by `scheduler/manager/002`.
//!
//! RED today: nothing captures the login-shell PATH, so the daemon's PATH lacks
//! the stub dir, the bare command is not found, the spawn fails, and the stub's
//! marker never appears.

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
    /// A fake login shell whose `-lc` output adds the stub dir to PATH.
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
///     and, invoked as `-lc '<script>'`, runs the script with that enriched
///     PATH — exactly what PRD #170's `$SHELL -lc 'printf %s "$PATH"'` capture
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
    // flag bundle (`-l`, `-c`, `-lc`, `-lic`, …) so `$SHELL -lc '<script>'`
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
/// at a fake login shell whose `-lc` output adds that dir to PATH. Open the
/// new-pane form (Ctrl+n → Space confirms the dir) — the Command field is
/// pre-filled with the bare stub — and submit via the `[Submit]` button. Assert
/// the bare command resolves and spawns: the stub writes its on-disk marker.
/// RED today: nothing captures the login-shell PATH, so the daemon's PATH lacks
/// the stub dir, the bare command is not found, the spawn fails, and the marker
/// never appears.
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
         the daemon should capture the login-shell PATH (`$SHELL -lc 'printf %s \
         \"$PATH\"'`) at startup so the bare command resolves"
    );
}

/// Scenario: Spawn a headless daemon with `$SHELL` pointed at a fake login shell
/// whose `-lc` output adds a stub dir (absent from the daemon's PATH) to PATH,
/// and register a scheduled task whose `command` is a bare stub living only in
/// that dir. Fire the task via the `RunNow` control message and assert the bare
/// command resolves and spawns: the stub writes its on-disk marker. RED today:
/// with no login-shell PATH capture the daemon's PATH lacks the stub dir, the
/// bare command is not found, and the marker never appears.
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
         the daemon should capture the login-shell PATH (`$SHELL -lc 'printf %s \
         \"$PATH\"'`) at startup so the bare command resolves"
    );
}
