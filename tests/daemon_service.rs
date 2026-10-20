//! Real-subprocess coverage for `dot-agent-deck daemon install` /
//! `daemon uninstall` on the systemd path (PRD #143; see `tests/CATALOG.md`'s
//! `daemon/service` section).
//!
//! The binary runs with a stand-in `systemctl` and `loginctl` first on `PATH`,
//! which record every call and answer `is-active` as the test says. The host's
//! own systemd user manager is never reached: these tests check what the
//! command writes and which service-manager calls it makes, which is where the
//! safety properties live — it never asks systemd to stop or restart anything.
//! The start-and-answer path, which needs a real daemon, is
//! `tests/e2e_daemon_service.rs`; the unit under a real systemd is the PRD's
//! recorded verification.
#![cfg(target_os = "linux")]

// Issue #322: fast-tier and does not link `tests/common/mod.rs`, so the
// crate's temp-root resolver is `#[path]`-included instead.
#[path = "../src/test_temp.rs"]
mod test_temp;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use spec::spec;

const SYSTEMCTL_STUB: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$STUB_LOG"
case "$*" in
  *is-active*) exit "${STUB_ACTIVE:-3}" ;;
  *FragmentPath*)
    printf '%s\n' "${STUB_FRAGMENT-$XDG_CONFIG_HOME/systemd/user/dot-agent-deck.service}" ;;
esac
exit 0
"#;

const LOGINCTL_STUB: &str = "#!/bin/sh\nprintf 'no\\n'\n";

/// An isolated `HOME` with the built deck installed at `~/.local/bin`, the
/// stand-ins on `PATH`, and sandbox endpoint overrides.
struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = test_temp::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let bin = root.join("home/.local/bin");
        std::fs::create_dir_all(&bin).expect("create ~/.local/bin");
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_dot-agent-deck"),
            bin.join("dot-agent-deck"),
        )
        .expect("install the built deck at ~/.local/bin");
        let stubs = root.join("stubs");
        std::fs::create_dir_all(&stubs).expect("create stub dir");
        for (name, body) in [("systemctl", SYSTEMCTL_STUB), ("loginctl", LOGINCTL_STUB)] {
            let path = stubs.join(name);
            std::fs::write(&path, body).expect("write stub");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        Self { _dir: dir, root }
    }

    fn installed_binary(&self) -> PathBuf {
        self.root.join("home/.local/bin/dot-agent-deck")
    }

    fn unit(&self) -> PathBuf {
        self.root.join("config/systemd/user/dot-agent-deck.service")
    }

    fn attach_socket(&self) -> PathBuf {
        self.root.join("attach.sock")
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("systemctl.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn run(&self, subcommand: &str, service_active: bool, extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = std::process::Command::new(self.installed_binary());
        cmd.args(["daemon", subcommand])
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("stubs").display()),
            )
            .env("DOT_AGENT_DECK_SOCKET", self.root.join("hook.sock"))
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", self.attach_socket())
            .env("DOT_AGENT_DECK_STATE_DIR", self.root.join("state"))
            .env("DOT_AGENT_DECK_LOG", self.root.join("deck.log"))
            .env("STUB_LOG", self.root.join("systemctl.log"))
            .env("STUB_ACTIVE", if service_active { "0" } else { "3" });
        for (name, value) in extra_env {
            cmd.env(name, value);
        }
        cmd.output().expect("run the real deck binary")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// No call may stop, restart or kill the unit: that would stop its agents.
fn assert_never_stops(calls: &[String]) {
    for call in calls {
        for word in [
            "--now",
            "stop",
            "restart",
            "kill",
            "try-restart",
            "reload-or-restart",
        ] {
            assert!(
                !call.split_whitespace().any(|w| w == word),
                "systemctl was asked to `{call}`, which stops a running daemon; calls={calls:?}"
            );
        }
    }
}

fn unit_line<'a>(unit: &'a str, prefix: &str) -> Vec<&'a str> {
    unit.lines().filter(|l| l.starts_with(prefix)).collect()
}

/// Scenario: With the deck installed at `~/.local/bin` in an isolated home, a stand-in `systemctl` that reports the service already active, and sandbox socket overrides set, run `dot-agent-deck daemon install`. It writes the unit under `$XDG_CONFIG_HOME/systemd/user/` running that installed path with the idle shutdown off and the socket overrides carried, enables it without starting or restarting anything, and prints the lingering advice the stand-in `loginctl` calls for.
#[spec("daemon/service/001")]
#[test]
fn daemon_service_001_install_writes_the_unit_and_leaves_a_running_service_running() {
    let sandbox = Sandbox::new();
    let out = sandbox.run("install", true, &[]);
    let stdout = text(&out.stdout);
    assert!(
        out.status.success(),
        "install failed: status={:?} stdout={stdout} stderr={}",
        out.status,
        text(&out.stderr)
    );

    let unit = std::fs::read_to_string(sandbox.unit()).expect("the unit was written");
    assert_eq!(
        unit_line(&unit, "ExecStart="),
        vec![
            format!(
                "ExecStart=\"{}\" daemon serve",
                sandbox.installed_binary().display()
            )
            .as_str()
        ],
        "the unit must run the installed path, not the cargo artifact behind it; unit:\n{unit}"
    );
    let env = unit_line(&unit, "Environment=");
    for expected in [
        "Environment=\"DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0\"".to_string(),
        format!(
            "Environment=\"DOT_AGENT_DECK_ATTACH_SOCKET={}\"",
            sandbox.attach_socket().display()
        ),
        format!(
            "Environment=\"DOT_AGENT_DECK_SOCKET={}\"",
            sandbox.root.join("hook.sock").display()
        ),
        format!(
            "Environment=\"DOT_AGENT_DECK_STATE_DIR={}\"",
            sandbox.root.join("state").display()
        ),
        format!(
            "Environment=\"XDG_CONFIG_HOME={}\"",
            sandbox.root.join("config").display()
        ),
    ] {
        assert!(
            env.contains(&expected.as_str()),
            "missing {expected}; unit:\n{unit}"
        );
    }
    assert_eq!(
        env.len(),
        5,
        "only the idle setting and the shell's endpoint and directory overrides are carried: {env:?}"
    );
    assert_eq!(unit_line(&unit, "Restart="), vec!["Restart=on-failure"]);
    assert_eq!(
        unit_line(&unit, "WantedBy="),
        vec!["WantedBy=default.target"]
    );

    let calls = sandbox.calls();
    assert!(
        calls.contains(&"--user daemon-reload".to_string()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"--user enable dot-agent-deck.service".to_string()),
        "{calls:?}"
    );
    assert_never_stops(&calls);
    assert!(stdout.contains("already running"), "{stdout}");
    assert!(stdout.contains("enable-linger"), "{stdout}");
}

/// Scenario: Install the unit with a stand-in `systemctl`, then run `dot-agent-deck daemon uninstall` while the stand-in reports the service still active. The unit file is removed and the service disabled without being stopped, and the output says the daemon is still running and how to stop it; a second uninstall reports there is nothing installed and still succeeds.
#[spec("daemon/service/002")]
#[test]
fn daemon_service_002_uninstall_removes_the_unit_and_never_stops_the_daemon() {
    let sandbox = Sandbox::new();
    assert!(sandbox.run("install", true, &[]).status.success());
    assert!(sandbox.unit().exists());

    let out = sandbox.run("uninstall", true, &[]);
    let stdout = text(&out.stdout);
    assert!(
        out.status.success(),
        "uninstall failed: {stdout} {}",
        text(&out.stderr)
    );
    assert!(!sandbox.unit().exists(), "the unit file is still there");
    let calls = sandbox.calls();
    assert!(
        calls.contains(&"--user disable dot-agent-deck.service".to_string()),
        "{calls:?}"
    );
    assert_never_stops(&calls);
    assert!(stdout.contains("still running"), "{stdout}");
    assert!(stdout.contains("daemon stop"), "{stdout}");

    let again = sandbox.run("uninstall", false, &[]);
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert!(
        text(&again.stdout).contains("not installed"),
        "{}",
        text(&again.stdout)
    );
}

/// Scenario: Run `dot-agent-deck daemon install` with `DOT_AGENT_DECK_STATE_DIR` set to a relative path. It refuses, naming the variable, before writing a unit or calling `systemctl`, because a service would resolve that path from a different directory than the shell.
#[spec("daemon/service/003")]
#[test]
fn daemon_service_003_install_refuses_a_relative_override_before_touching_anything() {
    let sandbox = Sandbox::new();
    let out = sandbox.run("install", false, &[("DOT_AGENT_DECK_STATE_DIR", "state")]);
    let stderr = text(&out.stderr);
    assert!(
        !out.status.success(),
        "install accepted a relative override"
    );
    assert!(
        !stderr.contains("Usage:"),
        "clap did not recognise `daemon install`: {stderr}"
    );
    assert!(stderr.contains("DOT_AGENT_DECK_STATE_DIR"), "{stderr}");
    assert!(!sandbox.unit().exists(), "a unit was written anyway");
    assert!(
        sandbox.calls().is_empty(),
        "systemctl was called: {:?}",
        sandbox.calls()
    );
}

/// Scenario: Run `dot-agent-deck daemon install` with a stand-in `systemctl` that reports loading `dot-agent-deck.service` from `/etc/systemd/user/` rather than from the file the command just wrote. It fails, naming both paths, before enabling or starting anything, instead of starting a unit other than the one it wrote.
#[spec("daemon/service/005")]
#[test]
fn daemon_service_005_install_refuses_when_systemd_loads_another_unit_file() {
    let sandbox = Sandbox::new();
    let out = sandbox.run(
        "install",
        false,
        &[("STUB_FRAGMENT", "/etc/systemd/user/dot-agent-deck.service")],
    );
    let stderr = text(&out.stderr);
    assert!(
        !out.status.success(),
        "install accepted a unit systemd does not load"
    );
    assert!(
        stderr.contains("/etc/systemd/user/dot-agent-deck.service"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&sandbox.unit().display().to_string()),
        "{stderr}"
    );
    let calls = sandbox.calls();
    assert!(
        !calls.iter().any(|c| c.contains("enable")),
        "systemctl was asked to enable after the mismatch: {calls:?}"
    );
}
