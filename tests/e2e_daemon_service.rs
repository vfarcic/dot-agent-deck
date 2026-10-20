#![cfg(all(feature = "e2e", target_os = "linux"))]

//! PRD #143 — `dot-agent-deck daemon install` starts the service and does not
//! report success until the daemon it started answers where this shell looks,
//! and a second install beside that live daemon starts nothing.
//!
//! The service manager is a stand-in `systemctl` that plays systemd's part for
//! `enable --now`: it reads `ExecStart`, `Environment` and `WorkingDirectory`
//! from the unit the command just wrote and starts that `daemon serve` in the
//! background. So the daemon that answers is the one the generated unit
//! describes, while the host's own systemd user manager is never touched. The
//! write-and-enable decisions without a daemon are `tests/daemon_service.rs`;
//! the unit under a real systemd is the PRD's recorded verification.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use spec::spec;

/// systemd's part for `enable --now`. The daemon's idle shutdown is
/// overridden to a minute (the unit's own `0` comes first, and `env` keeps the
/// last value) so a daemon this test leaks by being killed exits by itself.
const SYSTEMCTL_STUB: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$STUB_DIR/systemctl.log"
case "$*" in
  *is-active*) exit 3 ;;
  *"enable --now"*)
    unit="$XDG_CONFIG_HOME/systemd/user/dot-agent-deck.service"
    bin=$(sed -n 's/^ExecStart="\(.*\)" daemon serve$/\1/p' "$unit")
    envs=$(sed -n 's/^Environment="\(.*\)"$/\1/p' "$unit")
    [ "$(sed -n 's/^WorkingDirectory=//p' "$unit")" = "~" ] && cd "$HOME"
    env $envs DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=60 "$bin" daemon serve \
      </dev/null >>"$STUB_DIR/serve.log" 2>&1 &
    echo $! > "$STUB_DIR/serve.pid"
    ;;
esac
exit 0
"#;

/// Stops the daemon the stand-in started, by its own pid (CLAUDE.md rule
/// 12's teardown bullet), however the test ends.
struct ServeGuard(PathBuf);

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        else {
            return;
        };
        // SAFETY: kill(2) on the pid the stand-in recorded; errors are ignored.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        if !common::wait_until(Duration::from_secs(10), || !common::process_running(pid)) {
            // SAFETY: as above.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn run(root: &Path, args: &[&str]) -> Output {
    std::process::Command::new(root.join("home/.local/bin/dot-agent-deck"))
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("stubs").display()),
        )
        .env("DOT_AGENT_DECK_SOCKET", root.join("hook.sock"))
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", root.join("attach.sock"))
        .env("DOT_AGENT_DECK_STATE_DIR", root.join("state"))
        .env("DOT_AGENT_DECK_LOG", root.join("deck.log"))
        .env("DOT_AGENT_DECK_EXPERIMENTAL", "0")
        .env(
            dot_agent_deck::daemon_attach::DAEMON_START_TIMEOUT_ENV,
            common::daemon_start_timeout_for_launch()
                .as_millis()
                .to_string(),
        )
        .env("STUB_DIR", root.join("stubs"))
        .output()
        .expect("run the real deck binary")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Scenario: In an isolated home with the deck installed at `~/.local/bin` and a stand-in `systemctl` that starts the generated unit's `daemon serve` on `enable --now`, run `dot-agent-deck daemon install` with nothing running. It reports the daemon running only once that daemon answers `daemon probe` at the sandbox endpoint; a second `daemon install` then sees the live daemon, enables the service without `--now` and says it was not started, so no second daemon is launched.
#[spec("daemon/service/004")]
#[test]
fn daemon_service_004_install_starts_the_service_and_never_a_second_daemon() {
    let dir = common::race_safe_tempdir();
    let root = dir.path();
    let bin = root.join("home/.local/bin");
    std::fs::create_dir_all(&bin).expect("create ~/.local/bin");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_dot-agent-deck"),
        bin.join("dot-agent-deck"),
    )
    .expect("install the built deck at ~/.local/bin");
    let stubs = root.join("stubs");
    std::fs::create_dir_all(&stubs).expect("create stub dir");
    for (name, body) in [
        ("systemctl", SYSTEMCTL_STUB),
        ("loginctl", "#!/bin/sh\necho yes\n"),
    ] {
        std::fs::write(stubs.join(name), body).expect("write stub");
        std::fs::set_permissions(stubs.join(name), std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }
    let _guard = ServeGuard(stubs.join("serve.pid"));

    let first = run(root, &["daemon", "install"]);
    let stdout = text(&first.stdout);
    assert!(
        first.status.success(),
        "install failed: stdout={stdout} stderr={} serve.log={}",
        text(&first.stderr),
        std::fs::read_to_string(stubs.join("serve.log")).unwrap_or_default()
    );
    assert!(
        stdout.contains("running as dot-agent-deck.service"),
        "{stdout}"
    );
    let probe = run(root, &["daemon", "probe"]);
    assert!(
        text(&probe.stdout).starts_with("running"),
        "install reported success but no daemon answers: {}",
        text(&probe.stdout)
    );
    let first_pid = std::fs::read_to_string(stubs.join("serve.pid")).expect("serve.pid");

    let second = run(root, &["daemon", "install"]);
    let stdout = text(&second.stdout);
    assert!(second.status.success(), "{stdout} {}", text(&second.stderr));
    assert!(stdout.contains("not started"), "{stdout}");
    let calls = std::fs::read_to_string(stubs.join("systemctl.log")).unwrap_or_default();
    assert_eq!(
        calls.lines().filter(|l| l.contains("--now")).count(),
        1,
        "the second install started the service again beside a live daemon: {calls}"
    );
    assert!(
        calls
            .lines()
            .any(|l| l == "--user enable dot-agent-deck.service"),
        "{calls}"
    );
    assert_eq!(
        std::fs::read_to_string(stubs.join("serve.pid")).expect("serve.pid"),
        first_pid,
        "a second daemon was started"
    );
}
