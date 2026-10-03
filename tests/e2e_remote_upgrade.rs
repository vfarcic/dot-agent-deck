#![cfg(all(feature = "e2e", unix))]

//! Credential-free CLI/connect upgrade coverage. SSH executes the real remote
//! shell command in an owned HOME; curl supplies an owned installed wrapper.
//! The old and new daemon execute the same retained Cargo build with different
//! debug build stamps. This proves policy and process replacement, not release
//! compatibility, real SSH authentication, or real-agent work.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::daemon_protocol::{AttachRequest, AttachResponse, CAP_RESTART_DAEMON};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use spec::spec;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(30);
const DECK: &str = "upgrade-fixture";
const OLD_BUILD: &str = "0.1.0-gupgradeold";
const NEW_BUILD: &str = "0.2.0-gupgradenew";
const LABELS: [&str; 3] = ["ordinary-live", "lead", "coder"];

fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn script(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn pid_file(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

struct Remote {
    dir: TempDir,
    child: Child,
    attach: PathBuf,
    installed: PathBuf,
    successor: PathBuf,
    agents: Vec<(String, i32)>,
    env: Vec<(String, String)>,
    version: String,
}

impl Remote {
    fn new() -> Self {
        common::init_test_env();
        let dir = common::harness_tempdir().expect("owned remote sandbox");
        let root = dir.path();
        let home = root.join("remote-home");
        let local = root.join("client-home");
        let stubs = root.join("stubs");
        let installed = home.join(".local/bin/dot-agent-deck");
        let retained = root.join("retained-build");
        let attach = root.join("remote-attach.sock");
        let successor = root.join("successor.pid");
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::create_dir_all(&local).unwrap();
        // A hard link retains the build without overwriting Cargo's inode.
        for path in [&installed, &retained] {
            if fs::hard_link(env!("CARGO_BIN_EXE_dot-agent-deck"), path).is_err() {
                fs::copy(env!("CARGO_BIN_EXE_dot-agent-deck"), path).unwrap();
            }
        }
        let version_output = Command::new(&retained).arg("--version").output().unwrap();
        let version = String::from_utf8(version_output.stdout)
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned();
        assert!(semver::Version::parse(&version).unwrap() > semver::Version::new(0, 1, 0));
        let path = format!("{}:{}", stubs.display(), std::env::var("PATH").unwrap());
        let remote_env = vec![
            ("HOME".to_owned(), home.display().to_string()),
            ("PATH".to_owned(), path.clone()),
            ("TERM".to_owned(), "xterm-256color".into()),
            (
                "DOT_AGENT_DECK_ATTACH_SOCKET".into(),
                attach.display().to_string(),
            ),
            (
                "DOT_AGENT_DECK_SOCKET".into(),
                root.join("remote-hook.sock").display().to_string(),
            ),
            (
                "DOT_AGENT_DECK_STATE_DIR".into(),
                root.join("remote-state").display().to_string(),
            ),
            (
                "DOT_AGENT_DECK_SCHEDULES".into(),
                root.join("schedules.toml").display().to_string(),
            ),
            ("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS".into(), "0".into()),
            ("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED".into(), "1".into()),
            ("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS".into(), "180".into()),
            ("DOT_AGENT_DECK_BUILD_ID_OVERRIDE".into(), OLD_BUILD.into()),
        ];
        let child = Command::new(&installed)
            .args(["daemon", "serve"])
            .env_clear()
            .envs(remote_env.iter().cloned())
            .current_dir(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("daemon.log")).unwrap())
            .process_group(0)
            .spawn()
            .unwrap();

        // The shim handles only the command after our unique SSH host. All
        // production command text is executed by /bin/sh inside the sandbox.
        // Fixed Homebrew locations are masked so host installs cannot leak in.
        let ssh_env = serde_json::to_value(
            remote_env
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>(),
        )
        .unwrap();
        script(
            &stubs.join("ssh"),
            &format!(
                "#!/usr/bin/env python3\nimport json, os, sys\nargs = sys.argv[1:]\ni = args.index('fixture.invalid')\ncommand = ' '.join(args[i+1:])\nwith open({}, 'a') as log: log.write(command + '\\n')\nfor prefix in ['/opt/homebrew', '/usr/local', '/home/linuxbrew/.linuxbrew']:\n    command = command.replace(prefix + '/bin/brew', {})\nenv = json.loads({})\nos.chdir(env['HOME'])\nos.execve('/bin/sh', ['sh', '-c', command], env)\n",
                serde_json::to_string(&root.join("ssh.log").display().to_string()).unwrap(),
                serde_json::to_string(&root.join("absent-brew").display().to_string()).unwrap(),
                serde_json::to_string(&ssh_env.to_string()).unwrap(),
            ),
        );
        script(
            &stubs.join("curl"),
            &format!(
                "#!/bin/sh\nif [ -e {} ]; then echo 'fixture download refused' >&2; exit 22; fi\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = -o ]; then out=$2; fi\n  shift\ndone\ncp {} \"$out\"\n",
                quoted(&root.join("fail-install")),
                quoted(&root.join("payload")),
            ),
        );
        script(&stubs.join("brew"), "#!/bin/sh\nexit 1\n");
        let wrapper = |old: bool| {
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'dot-agent-deck {}'; exit 0; fi\nif [ \"$1\" = hooks ]; then printf installed > {}; exit 0; fi\nif [ \"$1\" = daemon ] && [ \"$2\" = serve ]; then printf '%s\\n' \"$$\" > {}; fi\nexport DOT_AGENT_DECK_BUILD_ID_OVERRIDE={}\nexec {} \"$@\"\n",
                if old { "0.1.0" } else { &version },
                quoted(&root.join("hooks-installed")),
                quoted(&successor),
                if old { OLD_BUILD } else { NEW_BUILD },
                quoted(&retained),
            )
        };
        script(&root.join("payload"), &wrapper(false));
        let old_wrapper = wrapper(true);
        let registry = root.join("remotes.toml");
        fs::write(&registry, format!(
            "[[remotes]]\nname = \"{DECK}\"\ntype = \"ssh\"\nhost = \"fixture.invalid\"\nport = 22\nversion = \"0.1.0\"\nadded_at = \"2026-10-03T00:00:00Z\"\n"
        )).unwrap();
        let remote = Self {
            child,
            attach,
            installed,
            successor,
            agents: Vec::new(),
            version,
            env: vec![
                ("HOME".into(), local.display().to_string()),
                ("PATH".into(), path),
                ("TERM".into(), "xterm-256color".into()),
                (
                    "DOT_AGENT_DECK_REMOTES".into(),
                    registry.display().to_string(),
                ),
                (
                    "DOT_AGENT_DECK_ATTACH_SOCKET".into(),
                    root.join("client-attach.sock").display().to_string(),
                ),
                (
                    "DOT_AGENT_DECK_SOCKET".into(),
                    root.join("client-hook.sock").display().to_string(),
                ),
                (
                    "DOT_AGENT_DECK_STATE_DIR".into(),
                    root.join("client-state").display().to_string(),
                ),
            ],
            dir,
        };
        assert!(
            common::wait_until(WAIT, || remote.hello().is_some()),
            "old daemon must start"
        );
        assert_eq!(
            remote.hello().unwrap().build_version.as_deref(),
            Some(OLD_BUILD)
        );
        // Atomic replacement, never truncate the retained executable inode.
        let staged = remote.installed.with_extension("staged");
        script(&staged, &old_wrapper);
        fs::rename(staged, &remote.installed).unwrap();
        remote
    }

    fn hello(&self) -> Option<AttachResponse> {
        common::attach_request_on(
            &self.attach,
            &AttachRequest::Hello {
                client_version: dot_agent_deck::daemon_protocol::PROTOCOL_VERSION,
                client_build_version: None,
            },
        )
        .ok()
    }

    fn inventory(&self) -> AttachResponse {
        common::attach_request_on(&self.attach, &AttachRequest::ListAgents).unwrap()
    }

    fn seed_live(&mut self) {
        for (index, label) in LABELS.iter().enumerate() {
            let pane = format!("upgrade-pane-{index}");
            let pid = self.dir.path().join(format!("agent-{index}.pid"));
            let agent = self.dir.path().join(format!("agent-{index}.sh"));
            script(
                &agent,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\nexec cat\n",
                    quoted(&pid)
                ),
            );
            let cwd = self.dir.path().display().to_string();
            let response = common::attach_request_on(
                &self.attach,
                &AttachRequest::StartAgent {
                    command: Some(format!("/bin/sh {}", quoted(&agent))),
                    cwd: Some(cwd.clone()),
                    rows: 24,
                    cols: 80,
                    env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane)],
                    display_name: Some((*label).into()),
                    tab_membership: (index > 0).then(|| TabMembership::Orchestration {
                        name: "upgrade-team".into(),
                        role_index: index - 1,
                        role_name: (*label).into(),
                        is_start_role: index == 1,
                        orchestration_cwd: Some(cwd),
                        display_title: None,
                        orchestration_id: Some("upgrade-instance".into()),
                    }),
                    agent_type: None,
                    seed: None,
                    authoring_kind: None,
                },
            )
            .unwrap();
            assert!(response.ok, "spawn stand-in: {:?}", response.error);
            assert!(
                common::wait_until(WAIT, || pid_file(&pid).is_some()),
                "stand-in must start"
            );
            self.agents
                .push((response.id.unwrap(), pid_file(&pid).unwrap()));
        }
        assert_eq!(self.inventory().orchestration_roles.unwrap().len(), 2);
    }

    fn piped(&self, extra: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        command
            .args(["remote", "upgrade", DECK])
            .args(extra)
            .env_clear()
            .envs(self.env.iter().cloned())
            .current_dir(self.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let child = command.spawn().unwrap();
        let pid = child.id() as i32;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(common::load_scaled(WAIT)) {
            Ok(output) => output.unwrap(),
            Err(error) => {
                // SAFETY: this owned command established its own process group.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                panic!("piped upgrade must not wait for a question: {error}");
            }
        }
    }

    fn tty(&self, args: &[&str]) -> Terminal {
        Terminal::new(args, &self.env, self.dir.path())
    }

    fn assert_installed(&self) {
        assert!(
            self.dir.path().join("hooks-installed").exists(),
            "install must refresh hooks"
        );
        let output = Command::new(&self.installed)
            .arg("--version")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("dot-agent-deck {}", self.version)
        );
        let registry: toml::Value =
            toml::from_str(&fs::read_to_string(self.dir.path().join("remotes.toml")).unwrap())
                .unwrap();
        assert_eq!(
            registry["remotes"][0]["version"].as_str(),
            Some(self.version.as_str())
        );
    }

    fn assert_kept(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "keep must preserve the old daemon PID"
        );
        assert_eq!(
            self.hello().unwrap().build_version.as_deref(),
            Some(OLD_BUILD)
        );
        assert!(!self.successor.exists(), "keep must not launch a successor");
        let inventory = self.inventory();
        let mut actual: Vec<_> = inventory
            .agent_records
            .unwrap()
            .into_iter()
            .map(|agent| agent.id)
            .collect();
        let mut wanted: Vec<_> = self.agents.iter().map(|(id, _)| id.clone()).collect();
        actual.sort();
        wanted.sort();
        assert_eq!(actual, wanted, "keep must preserve every agent identity");
        if !self.agents.is_empty() {
            assert_eq!(inventory.orchestration_roles.unwrap().len(), 2);
        }
        for (_, pid) in &self.agents {
            assert!(common::process_running(*pid), "kept agent {pid} stopped");
        }
    }

    fn assert_restarted(&mut self) {
        let exited = {
            let child = std::cell::RefCell::new(&mut self.child);
            common::wait_until(WAIT, || child.borrow_mut().try_wait().unwrap().is_some())
        };
        assert!(exited, "restarted outcome must exit the old daemon");
        assert!(
            common::wait_until(WAIT, || self
                .hello()
                .is_some_and(|hello| hello.build_version.as_deref() == Some(NEW_BUILD))),
            "new build must answer at the same endpoint"
        );
        let next = pid_file(&self.successor).expect("installed wrapper must record successor PID");
        assert_ne!(next, self.child.id() as i32, "daemon PID must change");
        assert!(common::process_running(next));
        assert!(self.inventory().agent_records.unwrap().is_empty());
        assert!(self.inventory().orchestration_roles.unwrap().is_empty());
        for (_, pid) in &self.agents {
            assert!(
                common::wait_until(WAIT, || !common::process_running(*pid)),
                "restart left named agent {pid} alive"
            );
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // On RED, legacy connect may lazy-spawn through the retained binary
        // instead of our installed wrapper, so no successor PID is recorded.
        // Stop any daemon at this fixture's private endpoint as well.
        let _ = common::attach_request_on(&self.attach, &AttachRequest::StopDaemon { force: true });
        for pid in [Some(self.child.id() as i32), pid_file(&self.successor)]
            .into_iter()
            .flatten()
        {
            if common::process_running(pid) {
                // SAFETY: only this fixture's recorded daemon process groups.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        for (_, pid) in &self.agents {
            // SAFETY: only PIDs recorded by the fixture's own stand-ins.
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "SSH commands:\n{}\ndaemon log:\n{}",
                fs::read_to_string(self.dir.path().join("ssh.log")).unwrap_or_default(),
                fs::read_to_string(self.dir.path().join("daemon.log")).unwrap_or_default()
            );
        }
    }
}

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    text: Arc<Mutex<Vec<u8>>>,
    grid: Arc<Mutex<vt100::Parser>>,
    drained: Arc<AtomicBool>,
}

impl Terminal {
    fn new(args: &[&str], env: &[(String, String)], cwd: &Path) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 45,
                cols: 180,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
        command.args(args);
        command.env_clear();
        command.cwd(cwd);
        for (key, value) in env {
            command.env(key, value);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let text = Arc::new(Mutex::new(Vec::new()));
        let grid = Arc::new(Mutex::new(vt100::Parser::new(45, 180, 0)));
        let text_thread = Arc::clone(&text);
        let grid_thread = Arc::clone(&grid);
        let drained = Arc::new(AtomicBool::new(false));
        let drained_thread = Arc::clone(&drained);
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(size) = reader.read(&mut buffer) {
                if size == 0 {
                    break;
                }
                text_thread
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buffer[..size]);
                grid_thread.lock().unwrap().process(&buffer[..size]);
            }
            drained_thread.store(true, Ordering::Release);
        });
        Self {
            child,
            _master: pair.master,
            writer,
            text,
            grid,
            drained,
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned()
    }
    fn screen(&self) -> String {
        self.grid.lock().unwrap().screen().contents()
    }
    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }
    fn wait(&mut self, needle: &str) {
        assert!(
            common::wait_until(WAIT, || self.output().contains(needle)
                || self.screen().contains(needle)),
            "PTY must show {needle:?}; output:\n{}\ngrid:\n{}",
            self.output(),
            self.screen()
        );
    }
    fn success(&mut self) {
        let status = {
            let child = std::cell::RefCell::new(&mut self.child);
            let status = std::cell::RefCell::new(None);
            common::wait_until(WAIT, || {
                *status.borrow_mut() = child.borrow_mut().try_wait().unwrap();
                status.borrow().is_some()
            });
            status.into_inner()
        };
        assert!(
            status.is_some(),
            "CLI must exit; output:\n{}",
            self.output()
        );
        assert!(
            status.unwrap().success(),
            "CLI must exit 0; output:\n{}",
            self.output()
        );
        assert!(
            common::wait_until(WAIT, || self.drained.load(Ordering::Acquire)),
            "PTY output must drain after CLI exit"
        );
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(pid) = self.child.process_id() {
            // SAFETY: portable-pty gives the owned child its own session/group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

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
