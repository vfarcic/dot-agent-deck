#![cfg(all(feature = "e2e", unix))]

//! Credential-free CLI/connect upgrade coverage. SSH executes the real remote
//! shell command in an owned HOME; curl supplies an owned installed wrapper.
//! The old and new daemon execute the same retained Cargo build with different
//! debug build stamps. This proves policy and process replacement, not release
//! compatibility, real SSH authentication, or real-agent work.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use dot_agent_deck::daemon_protocol::{AttachResponse, CAP_RESTART_DAEMON};
use spec::spec;

#[path = "support/remote_upgrade.rs"]
mod upgrade_fixture;
use upgrade_fixture::*;

fn readable(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_restart_summary(text: &str, version: &str) {
    let lower = text.to_lowercase();
    assert!(
        lower.contains("restarted") && text.matches(version).count() >= 2,
        "upgrade must disclose a restarted outcome with from/to versions; output:\n{text}"
    );
    assert!(
        !text.contains("Restart now?"),
        "idle upgrade must not ask a question: {text}"
    );
}

fn assert_kept_summary(text: &str) {
    let lower = text.to_lowercase();
    assert!(
        lower.contains("installed")
            && (lower.contains("not restarted")
                || lower.contains("kept")
                || lower.contains("keeping")
                || lower.contains("still running")),
        "upgrade must disclose installed but kept current daemon: {text}"
    );
}

fn assert_live_prompt(terminal: &mut Terminal) {
    terminal.wait("Restart now?");
    let text = terminal.output();
    for label in LABELS {
        assert!(
            text.contains(label),
            "restart prompt must name {label}: {text}"
        );
    }
    for pane in ["upgrade-pane-0", "upgrade-pane-1", "upgrade-pane-2"] {
        assert!(
            text.contains(pane),
            "restart prompt must name {pane}: {text}"
        );
    }
    assert!(
        text.contains("upgrade-team")
            && text.to_lowercase().contains("orchestrator")
            && text.contains("Keep current daemon"),
        "restart prompt must disclose role-map loss and the safe default: {text}"
    );
}

/// Scenario: Upgrade an idle remote over a real PTY. The CLI installs, silently replaces the daemon process, and prints a restarted outcome naming from and to versions.
#[spec("remote/upgrade/001")]
#[test]
fn remote_upgrade_001_tty_idle_restarts_without_question() {
    let mut idle = Remote::new();
    let mut terminal = idle.tty(&["remote", "upgrade", DECK]);
    terminal.success();
    assert_restart_summary(&terminal.output(), &idle.version);
    idle.assert_installed();
    idle.assert_restarted();
}

/// Scenario: Upgrade remotes with ordinary and orchestration stand-ins under a real PTY. The prompt names every agent and role; Enter preserves every live process, while r explicitly stops the disclosed work and replaces the daemon.
#[spec("remote/upgrade/006")]
#[test]
fn remote_upgrade_006_tty_live_keep_and_restart_choices() {
    for answer in ["\n", "r\n"] {
        let mut remote = Remote::new();
        remote.seed_live();
        let mut terminal = remote.tty(&["remote", "upgrade", DECK]);
        assert_live_prompt(&mut terminal);
        remote.assert_installed();
        remote.assert_kept();
        terminal.send(answer);
        terminal.success();
        if answer == "\n" {
            assert_kept_summary(&terminal.output());
            remote.assert_kept();
        } else {
            remote.assert_restarted();
        }
    }
}

/// Scenario: Upgrade an idle remote while stdout is piped and no one can answer a question. The CLI still installs and silently restarts the daemon, reports from and to versions, and exits 0.
#[spec("remote/upgrade/007")]
#[test]
fn remote_upgrade_007_piped_idle_restarts_without_question() {
    let mut remote = Remote::new();
    let output = remote.piped(&[]);
    let text = readable(&output);
    assert!(
        output.status.success(),
        "idle non-TTY upgrade must exit 0: {text}"
    );
    remote.assert_installed();
    assert_restart_summary(&text, &remote.version);
    remote.assert_restarted();
}

/// Scenario: Pipe stdout while upgrading a remote with live agents and roles. The CLI exits 0 after installing, asks no question, keeps all live work and explicitly names what blocked the restart.
#[spec("remote/upgrade/002")]
#[test]
fn remote_upgrade_002_piped_live_installs_and_keeps() {
    let mut remote = Remote::new();
    remote.seed_live();
    let output = remote.piped(&[]);
    let text = readable(&output);
    assert!(
        output.status.success(),
        "non-TTY upgrade must exit 0: {text}"
    );
    remote.assert_installed();
    assert!(
        !text.contains("Restart now?"),
        "piped upgrade must not ask: {text}"
    );
    remote.assert_kept();
    assert_kept_summary(&text);
    for label in LABELS {
        assert!(
            text.contains(label),
            "piped outcome must name blocker {label}: {text}"
        );
    }
    assert!(
        text.contains("upgrade-team"),
        "piped outcome must name the blocked role map: {text}"
    );
}

/// Scenario: Request JSON output for idle and live upgrades and for an installation failure. Each invocation emits a single UpgradeOutcome with its kebab-case tag, versions, blockers or installing-stage reason, and an exit code matching that outcome.
#[spec("remote/upgrade/003")]
#[test]
fn remote_upgrade_003_json_outcomes_are_machine_readable() {
    for mode in ["idle", "live", "fail"] {
        let mut remote = Remote::new();
        if mode == "live" {
            remote.seed_live();
        }
        if mode == "fail" {
            fs::write(remote.dir.path().join("fail-install"), "fail").unwrap();
        }
        let output = remote.piped(&["--json"]);
        assert_eq!(
            output.status.success(),
            mode != "fail",
            "JSON outcome exit status: {}",
            readable(&output)
        );
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "--json must emit only one UpgradeOutcome: {error}; {}",
                    readable(&output)
                )
            });
        match mode {
            "idle" => {
                assert_eq!(value["outcome"], "restarted");
                assert_eq!(value["from_version"], remote.version);
                assert_eq!(value["to_version"], remote.version);
                remote.assert_restarted();
            }
            "live" => {
                assert_eq!(value["outcome"], "installed-not-restarted");
                assert_eq!(value["reason"]["kind"], "no-one-to-ask");
                assert_eq!(value["installed_version"], remote.version);
                for label in LABELS {
                    assert!(
                        value["reason"]["at_stake"]["agents"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|agent| agent["label"] == label),
                        "JSON must name blocker {label}: {value}"
                    );
                }
                assert_eq!(
                    value["reason"]["at_stake"]["agents"]
                        .as_array()
                        .unwrap()
                        .len(),
                    3
                );
                assert_eq!(
                    value["reason"]["at_stake"]["roles"]
                        .as_array()
                        .unwrap()
                        .len(),
                    2
                );
                remote.assert_kept();
            }
            _ => {
                assert_eq!(value["outcome"], "failed");
                assert_eq!(value["stage"], "installing");
                assert!(
                    value["reason"]
                        .as_str()
                        .unwrap()
                        .contains("fixture download refused")
                );
                remote.assert_kept();
            }
        }
    }
}

struct OldPeer {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl OldPeer {
    fn replace(remote: &mut Remote) -> Self {
        remote.child.kill().unwrap();
        remote.child.wait().unwrap();
        fs::remove_file(&remote.attach).unwrap();
        let listener = UnixListener::bind(&remote.attach).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut header = [0; 5];
                if stream.read_exact(&mut header).is_err() {
                    break;
                }
                assert_eq!(header[0], dot_agent_deck::daemon_protocol::KIND_REQ);
                let size = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
                assert!(size <= 65536, "bounded scripted request");
                let mut payload = vec![0; size];
                stream.read_exact(&mut payload).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                log.lock()
                    .unwrap()
                    .push(request["op"].as_str().unwrap().to_owned());
                let mut hello =
                    AttachResponse::hello(dot_agent_deck::daemon_protocol::PROTOCOL_VERSION)
                        .with_capabilities();
                hello
                    .capabilities
                    .as_mut()
                    .unwrap()
                    .retain(|cap| cap != CAP_RESTART_DAEMON);
                hello.daemon_version = Some("0.1.0".into());
                let payload = serde_json::to_vec(&hello).unwrap();
                header[0] = dot_agent_deck::daemon_protocol::KIND_RESP;
                header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
                stream.write_all(&header).unwrap();
                stream.write_all(&payload).unwrap();
            }
        });
        Self {
            socket: remote.attach.clone(),
            requests,
            thread: Some(thread),
        }
    }
}

impl Drop for OldPeer {
    fn drop(&mut self) {
        if let Ok(stream) = UnixStream::connect(&self.socket) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Scenario: Install a new binary while a scripted older daemon omits restart-daemon from its advertised capabilities. The CLI exits 0, tells the user the daemon is too old to restart itself and gives a remedy; the older socket peer receives only Hello frames and remains reachable.
#[spec("remote/upgrade/004")]
#[test]
fn remote_upgrade_004_old_capability_is_kept_without_restart_frame() {
    let mut remote = Remote::new();
    let old = OldPeer::replace(&mut remote);
    let output = remote.piped(&[]);
    let text = readable(&output);
    assert!(
        output.status.success(),
        "old daemon outcome must exit 0: {text}"
    );
    remote.assert_installed();
    let cli_requests = old.requests.lock().unwrap().clone();
    assert_eq!(
        remote.hello().unwrap().daemon_version.as_deref(),
        Some("0.1.0")
    );
    assert!(!remote.successor.exists());
    let requests = old.requests.lock().unwrap().clone();
    assert!(
        requests.iter().all(|op| op == "hello"),
        "too-old peer must never receive restart or stop: {:?}",
        requests
    );
    assert!(
        text.to_lowercase().contains("too old to restart itself"),
        "installed outcome must explain old daemon capability: {text}"
    );
    assert!(
        text.to_lowercase().contains("installed")
            && [
                "daemon stop",
                "daemon restart",
                "connect",
                "restart manually",
                "manually restart",
                "stop and start"
            ]
            .iter()
            .any(|remedy| text.to_lowercase().contains(remedy)),
        "too-old outcome needs a remedy: {text}"
    );
    assert!(
        !cli_requests.is_empty(),
        "the CLI must probe the running old daemon before declaring it too old"
    );
}

/// Scenario: Make the sandbox download fail while live agents and roles run. The CLI exits nonzero with an installing-stage failure and the download reason, without changing the installed file, daemon PID, agents or role map.
#[spec("remote/upgrade/005")]
#[test]
fn remote_upgrade_005_install_failure_preserves_live_daemon() {
    let mut remote = Remote::new();
    remote.seed_live();
    let before = fs::read(&remote.installed).unwrap();
    fs::write(remote.dir.path().join("fail-install"), "fail").unwrap();
    let output = remote.piped(&[]);
    let text = readable(&output);
    assert!(
        !output.status.success(),
        "failed install must exit nonzero: {text}"
    );
    assert_eq!(fs::read(&remote.installed).unwrap(), before);
    remote.assert_kept();
    assert!(
        text.contains("fixture download refused"),
        "installation reason must be visible: {text}"
    );
    assert!(
        text.to_lowercase().contains("install") && text.to_lowercase().contains("fail"),
        "failure must identify the installing stage: {text}"
    );
}

/// Scenario: Connect over a PTY to an older remote with live agents and answer y at Upgrade and connect. Enter at the named restart question preserves the old daemon and its agents and renders its orchestration tab, while r restarts onto the installed build and renders the empty dashboard; neither asks a second remote handshake question.
#[spec("remote/connect/001")]
#[test]
fn remote_connect_001_upgrade_choices_connect_without_second_question() {
    for mode in ["live-keep", "live-restart"] {
        let mut remote = Remote::new();
        remote.seed_live();
        let mut terminal = remote.tty(&["connect", DECK]);
        terminal.wait("Upgrade and connect? [y/N]");
        terminal.send("y\n");
        assert_live_prompt(&mut terminal);
        remote.assert_installed();
        remote.assert_kept();
        terminal.send(if mode == "live-keep" { "\n" } else { "r\n" });
        // A kept orchestration reopens on its tab, while a restarted daemon
        // opens the empty dashboard. Require rendered TUI chrome as well as
        // the expected content: the raw restart prompt also names the roles.
        assert!(
            common::wait_until(WAIT, || {
                let grid = terminal.screen();
                grid.contains("[New Agent Ctrl+N]")
                    && if mode == "live-keep" {
                        grid.contains("Dashboard")
                            && grid.contains("upgrade-team [×]")
                            && grid.contains("lead")
                            && grid.contains("coder")
                    } else {
                        grid.contains("No active agents")
                    }
            }),
            "{mode}: connect must render the attached daemon without a second handshake question; output:\n{}\ngrid:\n{}",
            terminal.output(),
            terminal.screen()
        );
        let text = terminal.output();
        assert_eq!(text.matches("Upgrade and connect? [y/N]").count(), 1);
        assert_eq!(
            text.matches("Restart now?").count(),
            1,
            "connect must ask for restart only once: {text}"
        );
        assert!(
            !text.contains("[s]") && !text.contains("press s"),
            "remote attach must not repeat consent: {text}"
        );
        remote.assert_installed();
        if mode == "live-keep" {
            remote.assert_kept();
            assert_kept_summary(&text);
        } else {
            assert!(
                text.to_lowercase().contains("restarted"),
                "connect must show shared restarted outcome: {text}"
            );
            remote.assert_restarted();
        }
    }
}

/// Scenario: Connect over a PTY to an older idle remote and answer y at Upgrade and connect. Installation and silent shared restart lead to the new daemon's empty dashboard with a visible restarted outcome and no second handshake question.
#[spec("remote/connect/003")]
#[test]
fn remote_connect_003_idle_upgrade_connects_to_new_daemon() {
    let mut remote = Remote::new();
    let mut terminal = remote.tty(&["connect", DECK]);
    terminal.wait("Upgrade and connect? [y/N]");
    terminal.send("y\n");
    terminal.wait("No active agents");
    remote.assert_installed();
    remote.assert_restarted();
    let text = terminal.output();
    assert!(
        text.to_lowercase().contains("restarted"),
        "connect must show shared restarted outcome: {text}"
    );
    assert_eq!(text.matches("Upgrade and connect? [y/N]").count(), 1);
    assert!(
        !text.contains("Restart now?") && !text.contains("[s]"),
        "idle connect must not ask a second question: {text}"
    );
}

/// Scenario: Connect to an older idle remote and decline Upgrade and connect with N or Enter. The existing daemon and installed file remain unchanged, no download or hook installation runs, and the remote dashboard still opens.
#[spec("remote/connect/002")]
#[test]
fn remote_connect_002_declining_upgrade_preserves_existing_behavior() {
    for answer in ["N\n", "\n"] {
        let mut remote = Remote::new();
        let before = fs::read(&remote.installed).unwrap();
        let mut terminal = remote.tty(&["connect", DECK]);
        terminal.wait("Upgrade and connect? [y/N]");
        terminal.send(answer);
        terminal.wait("No active agents");
        remote.assert_kept();
        assert_eq!(fs::read(&remote.installed).unwrap(), before);
        assert!(!remote.dir.path().join("hooks-installed").exists());
        assert!(!terminal.output().contains("Restart now?"));
    }
}
