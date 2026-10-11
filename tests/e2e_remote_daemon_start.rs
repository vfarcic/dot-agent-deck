#![cfg(all(feature = "e2e", unix))]

//! Shared start functions against the real daemon, with SSH commands executed
//! by /bin/sh in an owned HOME. No SSH server or agent credential is needed.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use dot_agent_deck::daemon_client::{LocalEndpoint, RemoteEndpoint};
use dot_agent_deck::daemon_protocol::{AttachRequest, AttachResponse, PROTOCOL_VERSION};
use dot_agent_deck::daemon_start::{
    DisconnectedAction, DisconnectedReason, RemoteDeck, StartFailure, StartOutcome, StartTiming,
    probe_local, probe_remote, start_local, start_remote,
};
use dot_agent_deck::remote::{RemoteBinaryPath, RemoteEntry, RemotesFile, SystemSshExecutor};
use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};
use spec::spec;
use tempfile::TempDir;

const CHILD_ROOT: &str = "DOT_AGENT_DECK_START_TEST_ROOT";
const WAIT: Duration = Duration::from_secs(20);

fn script(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn socket(&self) -> PathBuf {
        // Distinct from the remote environment's default, so inheriting the
        // environment instead of passing the deck's socket fails the test.
        self.root.join("configured-deck.sock")
    }

    fn installed(&self) -> PathBuf {
        self.root.join("remote-home/.local/bin/dot-agent-deck")
    }

    fn deck(&self) -> RemoteDeck<SystemSshExecutor> {
        let endpoint = RemoteEndpoint::new(
            Hostname::parse("fixture.invalid").unwrap(),
            RemoteSocketPath::parse(self.socket().to_str().unwrap()).unwrap(),
        );
        let binary = RemoteBinaryPath::try_from(self.installed().to_str().unwrap().to_string())
            .expect("the sandbox install path is a safe remote binary path");
        RemoteDeck::for_endpoint(&endpoint, Some(&binary))
    }

    /// Where the Homebrew stand-in installs the deck, and its `brew`.
    fn brew_prefix(&self) -> PathBuf {
        self.root.join("brew")
    }

    fn brew_bin(&self) -> PathBuf {
        self.root.join("remote-path")
    }

    /// The deck as a deck-list row that records no binary — a legacy row, or
    /// one the desktop added — so it runs the default install, recording what
    /// a check finds in its place in the deck list at `remotes`.
    fn unrecorded_deck(&self, remotes: &Path) -> RemoteDeck<SystemSshExecutor> {
        let endpoint = RemoteEndpoint::new(
            Hostname::parse("fixture.invalid").unwrap(),
            RemoteSocketPath::parse(self.socket().to_str().unwrap()).unwrap(),
        );
        let entry = RemoteEntry {
            name: "studio".into(),
            kind: "ssh".into(),
            host: "fixture.invalid".into(),
            port: 22,
            key: None,
            version: "0.40.0".into(),
            added_at: "2026-10-01T00:00:00Z".into(),
            upgraded_at: None,
            last_connected: None,
            install: None,
            binary: None,
            id: None,
            user: None,
            jump_host: None,
            socket: Some(self.socket().display().to_string()),
        };
        RemotesFile {
            remotes: vec![entry.clone()],
        }
        .save(remotes)
        .unwrap();
        RemoteDeck::for_endpoint(&endpoint, None).recording_to(remotes.into(), entry)
    }

    fn timing(&self) -> StartTiming {
        StartTiming {
            wait: WAIT,
            poll: Duration::from_millis(50),
        }
    }

    fn daemon_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "HOME".into(),
                self.root.join("remote-home").display().to_string(),
            ),
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", self.brew_bin().display()),
            ),
            ("SHELL".into(), "/bin/sh".into()),
            (
                "DOT_AGENT_DECK_ATTACH_SOCKET".into(),
                self.root
                    .join("inherited-default.sock")
                    .display()
                    .to_string(),
            ),
            (
                "DOT_AGENT_DECK_SOCKET".into(),
                self.root.join("hook.sock").display().to_string(),
            ),
            (
                "DOT_AGENT_DECK_STATE_DIR".into(),
                self.root.join("state").display().to_string(),
            ),
            (
                "DOT_AGENT_DECK_LOG".into(),
                self.root.join("daemon.log").display().to_string(),
            ),
            (
                "DOT_AGENT_DECK_SCHEDULES".into(),
                self.root.join("schedules.toml").display().to_string(),
            ),
            ("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS".into(), "60".into()),
            // Detached remote children are intentionally orphaned by ssh.
            ("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED".into(), "0".into()),
            ("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS".into(), "120".into()),
        ];
        env.extend(common::config_containment_env().map(|(key, value)| (key.into(), value)));
        env
    }

    fn hello(&self) -> AttachResponse {
        common::attach_request_on(
            &self.socket(),
            &AttachRequest::Hello {
                client_version: PROTOCOL_VERSION,
                client_build_version: None,
            },
        )
        .expect("the started daemon must answer Hello at the configured deck socket")
    }

    fn pids(&self) -> Vec<i32> {
        fs::read_to_string(self.root.join("daemon.pids"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().expect("recorded daemon PID"))
            .collect()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }

    fn assert_one_daemon(&self) {
        let pids = self.pids();
        assert_eq!(pids.len(), 1, "exactly one daemon spawn: {pids:?}");
        assert!(
            common::process_running(pids[0]),
            "recorded daemon must stay alive"
        );
        let log = self.log();
        assert_eq!(
            log.matches("Attach protocol listening").count(),
            1,
            "exactly one listener:\n{log}"
        );
        assert!(
            !self.root.join("inherited-default.sock").exists(),
            "starting must use the deck's configured socket, not the inherited default"
        );
    }
}

struct OwnedSandbox {
    sandbox: Sandbox,
    _dir: TempDir,
}

impl OwnedSandbox {
    fn new() -> Self {
        common::init_test_env();
        let dir = common::harness_tempdir().unwrap();
        let sandbox = Sandbox {
            root: dir.path().to_owned(),
        };
        fs::create_dir_all(sandbox.installed().parent().unwrap()).unwrap();
        fs::create_dir_all(sandbox.root.join("stubs")).unwrap();
        fs::create_dir_all(sandbox.root.join("client-home")).unwrap();
        script(
            &sandbox.installed(),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = daemon ] && [ \"$2\" = serve ]; then printf '%s\\n' \"$$\" >> {}; fi\nexec {} \"$@\"\n",
                quoted(&sandbox.root.join("daemon.pids")),
                quoted(Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"))),
            ),
        );
        let remote_env: std::collections::BTreeMap<_, _> =
            sandbox.daemon_env().into_iter().collect();
        // Match the upgrade fixture: execute the production command following
        // our unique host under /bin/sh with a completely owned environment.
        script(
            &sandbox.root.join("stubs/ssh"),
            &format!(
                "#!/usr/bin/python3\nimport json, os, sys\nargs = sys.argv[1:]\ni = args.index('fixture.invalid')\ncommand = ' '.join(args[i+1:])\nif os.path.exists({}):\n    print('ssh: connect to host fixture.invalid port 22: Connection refused', file=sys.stderr)\n    sys.exit(255)\nwith open({}, 'a') as log: log.write(command + '\\n')\nenv = json.loads({})\nos.chdir(env['HOME'])\nos.execve('/bin/sh', ['sh', '-c', command], env)\n",
                serde_json::to_string(&sandbox.root.join("unreachable")).unwrap(),
                serde_json::to_string(&sandbox.root.join("ssh.log")).unwrap(),
                serde_json::to_string(&serde_json::to_string(&remote_env).unwrap()).unwrap(),
            ),
        );
        Self { sandbox, _dir: dir }
    }
}

impl Drop for OwnedSandbox {
    fn drop(&mut self) {
        for pid in self.sandbox.pids() {
            if common::process_running(pid) {
                // SAFETY: PID came from this sandbox's daemon wrapper only.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                if !common::wait_until(Duration::from_secs(3), || !common::process_running(pid)) {
                    // SAFETY: the same owned daemon PID, after bounded drain.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

fn in_sandbox(name: &str, scenario: impl FnOnce(&Sandbox)) {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        scenario(&Sandbox { root: root.into() });
        return;
    }
    let owned = OwnedSandbox::new();
    let root = &owned.sandbox.root;
    // Re-enter only this test in a child with an isolated PATH. This exercises
    // RemoteDeck::for_endpoint and SystemSshExecutor without mutating the
    // multithreaded test runner's process environment.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env_clear()
        .env(CHILD_ROOT, root)
        .env("HOME", root.join("client-home"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("stubs").display()),
        )
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", root.join("client.sock"))
        .env("DOT_AGENT_DECK_SOCKET", root.join("client-hook.sock"))
        .env("DOT_AGENT_DECK_STATE_DIR", root.join("client-state"))
        .env("DOT_AGENT_DECK_LOG", root.join("client.log"))
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "the child must execute exactly the requested scenario {name}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.status.success(),
        "sandboxed scenario failed:\nstdout:\n{}\nstderr:\n{}\ndaemon log:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        owned.sandbox.log(),
    );
}

/// Scenario: Check a stopped remote deck, start its real daemon over the sandbox SSH shim, and exchange Hello at a deliberately non-default socket. The start reports Started and subsequent checks report a running daemon.
#[spec("remote/start/001")]
#[test]
fn remote_start_001_stopped_daemon_starts_at_configured_socket() {
    in_sandbox(
        "remote_start_001_stopped_daemon_starts_at_configured_socket",
        |sandbox| {
            let deck = sandbox.deck();
            let reason = probe_remote(&deck);
            assert_eq!(reason, DisconnectedReason::NotRunning);
            assert_eq!(reason.action(), DisconnectedAction::StartDaemon);
            assert_eq!(start_remote(&deck, sandbox.timing()), StartOutcome::Started);
            assert_eq!(probe_remote(&deck), DisconnectedReason::RunningNotConnected);
            assert_eq!(sandbox.hello().server_version, Some(PROTOCOL_VERSION));
            sandbox.assert_one_daemon();
        },
    );
}

/// Scenario: Start a real remote daemon and request another start at the same socket. The second request reports AlreadyRunning, the same daemon answers Hello, and only one daemon spawn and attach listener are recorded.
#[spec("remote/start/002")]
#[test]
fn remote_start_002_running_daemon_is_not_spawned_again() {
    in_sandbox(
        "remote_start_002_running_daemon_is_not_spawned_again",
        |sandbox| {
            let deck = sandbox.deck();
            assert_eq!(start_remote(&deck, sandbox.timing()), StartOutcome::Started);
            let pids = sandbox.pids();
            assert_eq!(
                start_remote(&deck, sandbox.timing()),
                StartOutcome::AlreadyRunning
            );
            assert_eq!(
                sandbox.pids(),
                pids,
                "another start must preserve the daemon process"
            );
            assert_eq!(probe_remote(&deck).action(), DisconnectedAction::Reconnect);
            assert_eq!(sandbox.hello().server_version, Some(PROTOCOL_VERSION));
            sandbox.assert_one_daemon();
        },
    );
}

/// Scenario: Remove the remote install, then simulate an unreachable SSH host. Checks and starts classify each failure, point missing-install users at remote add, name the unreachable host, and spawn no daemon.
#[spec("remote/start/003")]
#[test]
fn remote_start_003_missing_install_and_unreachable_host_are_explained() {
    in_sandbox(
        "remote_start_003_missing_install_and_unreachable_host_are_explained",
        |sandbox| {
            fs::remove_file(sandbox.installed()).unwrap();
            let deck = sandbox.deck();
            for (failure, message) in [
                (StartFailure::NotInstalled, "dot-agent-deck remote add"),
                (
                    StartFailure::HostUnreachable,
                    "cannot reach fixture.invalid over ssh",
                ),
            ] {
                if failure == StartFailure::HostUnreachable {
                    fs::write(sandbox.root.join("unreachable"), "refused").unwrap();
                }
                let reason = probe_remote(&deck);
                assert_eq!(reason.action(), DisconnectedAction::Reconnect);
                let DisconnectedReason::Unknown(problem) = reason else {
                    panic!("expected unknown reason: {reason:?}")
                };
                assert_eq!(problem.failure, failure, "{problem:?}");
                assert!(problem.message.contains(message), "{}", problem.message);
                let outcome = start_remote(&deck, sandbox.timing());
                let StartOutcome::Failed(problem) = outcome else {
                    panic!("expected start failure: {outcome:?}")
                };
                assert_eq!(problem.failure, failure, "{problem:?}");
                assert!(problem.message.contains(message), "{}", problem.message);
            }
            assert!(
                sandbox.pids().is_empty(),
                "failed starts must spawn no daemon"
            );
        },
    );
}

/// Scenario: A host whose only install is Homebrew's, and a deck-list row that records no binary. The desktop's check finds the Homebrew binary instead of saying "not installed", Start daemon starts the real daemon through it at the configured socket, and the row now records the Homebrew binary.
#[spec("remote/start/004")]
#[test]
fn remote_start_004_homebrew_only_host_is_found_started_and_recorded() {
    in_sandbox(
        "remote_start_004_homebrew_only_host_is_found_started_and_recorded",
        |sandbox| {
            // Move the install from ~/.local/bin to a Homebrew prefix, and put
            // a `brew` that owns it on the remote shell's PATH.
            let brewed = sandbox.brew_prefix().join("bin/dot-agent-deck");
            fs::create_dir_all(brewed.parent().unwrap()).unwrap();
            fs::rename(sandbox.installed(), &brewed).unwrap();
            fs::create_dir_all(sandbox.brew_bin()).unwrap();
            let prefix = quoted(&sandbox.brew_prefix());
            script(
                &sandbox.brew_bin().join("brew"),
                &format!(
                    "#!/bin/sh\ncase \"$*\" in\n'--prefix'|'--prefix dot-agent-deck') echo {prefix} ;;\n'list --formula dot-agent-deck') exit 0 ;;\n*) exit 1 ;;\nesac\n"
                ),
            );
            let remotes = sandbox.root.join("client-home/remotes.toml");
            let deck = sandbox.unrecorded_deck(&remotes);

            let reason = probe_remote(&deck);
            assert_eq!(reason, DisconnectedReason::NotRunning, "{reason:?}");
            assert_eq!(reason.action(), DisconnectedAction::StartDaemon);
            let row = RemotesFile::load(&remotes).unwrap().remotes.remove(0);
            assert_eq!(row.install.as_deref(), Some("homebrew"));
            assert_eq!(
                row.binary.as_ref().map(|binary| binary.as_str()),
                brewed.to_str()
            );

            // The start reads the row again, as the desktop does, and runs
            // the recorded binary straight away.
            let endpoint = RemoteEndpoint::new(
                Hostname::parse("fixture.invalid").unwrap(),
                RemoteSocketPath::parse(sandbox.socket().to_str().unwrap()).unwrap(),
            );
            let deck = RemoteDeck::for_endpoint(&endpoint, row.binary.as_ref());
            assert_eq!(start_remote(&deck, sandbox.timing()), StartOutcome::Started);
            assert_eq!(probe_remote(&deck), DisconnectedReason::RunningNotConnected);
            assert_eq!(sandbox.hello().server_version, Some(PROTOCOL_VERSION));
            sandbox.assert_one_daemon();
            let ssh_log = fs::read_to_string(sandbox.root.join("ssh.log")).unwrap();
            assert!(
                !ssh_log.contains(".local/bin/dot-agent-deck daemon serve"),
                "the start must run the Homebrew binary:\n{ssh_log}"
            );
        },
    );
}

/// Scenario: Start a real local daemon through the shared start function at an owned socket, then request another start. The daemon answers Hello, its probe reports running, and the second request reports AlreadyRunning without invoking spawn.
#[spec("lifecycle/daemon-start/001")]
#[test]
fn daemon_start_001_real_daemon_starts_once() {
    in_sandbox("daemon_start_001_real_daemon_starts_once", |sandbox| {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let endpoint = LocalEndpoint::at(sandbox.socket());
        let mut child = None;
        runtime.block_on(async {
            assert_eq!(probe_local(&endpoint).await, DisconnectedReason::NotRunning);
            let outcome = start_local(
                &endpoint,
                &sandbox.root.join("state"),
                || {
                    child = Some(
                        Command::new(sandbox.installed())
                            .args(["daemon", "serve"])
                            .env_clear()
                            .envs(sandbox.daemon_env())
                            .env("DOT_AGENT_DECK_ATTACH_SOCKET", sandbox.socket())
                            .current_dir(sandbox.root.join("remote-home"))
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()?,
                    );
                    Ok(())
                },
                Duration::from_millis(25),
                WAIT,
            )
            .await;
            assert_eq!(outcome, StartOutcome::Started);
            assert_eq!(
                probe_local(&endpoint).await,
                DisconnectedReason::RunningNotConnected
            );
            assert_eq!(
                start_local(
                    &endpoint,
                    &sandbox.root.join("state"),
                    || panic!("a running daemon must not invoke spawn again"),
                    Duration::from_millis(25),
                    WAIT,
                )
                .await,
                StartOutcome::AlreadyRunning
            );
            assert_eq!(sandbox.hello().server_version, Some(PROTOCOL_VERSION));
            sandbox.assert_one_daemon();
        });
        // This local child is owned directly as well as recorded for panic cleanup.
        let mut child = child.unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    });
}
