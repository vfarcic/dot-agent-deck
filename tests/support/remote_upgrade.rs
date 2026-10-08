//! Owned SSH/install stand-ins shared by deterministic and real-agent upgrade scenarios.
#![allow(dead_code)]

use crate::common;
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::daemon_protocol::{AttachRequest, AttachResponse};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tempfile::TempDir;

pub(crate) const WAIT: Duration = Duration::from_secs(30);
pub(crate) const DECK: &str = "upgrade-fixture";
/// The version the fixture's pre-upgrade install reports. Below every real
/// build, including the `0.1.0` placeholder a tagless checkout reports.
pub(crate) const OLD_VERSION: &str = "0.0.1";
pub(crate) const OLD_BUILD: &str = "0.0.1-gupgradeold";
pub(crate) const NEW_BUILD: &str = "0.2.0-gupgradenew";
pub(crate) const LABELS: [&str; 3] = ["ordinary-live", "lead", "coder"];

pub(crate) fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

pub(crate) fn script(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

pub(crate) fn pid_file(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(crate) struct Remote {
    pub(crate) dir: TempDir,
    pub(crate) child: Child,
    pub(crate) attach: PathBuf,
    pub(crate) installed: PathBuf,
    pub(crate) successor: PathBuf,
    pub(crate) agents: Vec<(String, i32)>,
    expected_roles: usize,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) version: String,
}

impl Remote {
    pub(crate) fn new() -> Self {
        Self::with_lifetime(180)
    }

    pub(crate) fn with_lifetime(seconds: u64) -> Self {
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
        assert!(
            semver::Version::parse(&version).unwrap()
                > semver::Version::parse(OLD_VERSION).unwrap(),
            "the build under test ({version}) must be newer than the fixture's old install"
        );
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
            (
                "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS".into(),
                seconds.to_string(),
            ),
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
                if old { OLD_VERSION } else { &version },
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
            "[[remotes]]\nname = \"{DECK}\"\ntype = \"ssh\"\nhost = \"fixture.invalid\"\nport = 22\nversion = \"{OLD_VERSION}\"\nadded_at = \"2026-10-03T00:00:00Z\"\n"
        )).unwrap();
        let remote = Self {
            child,
            attach,
            installed,
            successor,
            agents: Vec::new(),
            expected_roles: 0,
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

    pub(crate) fn hello(&self) -> Option<AttachResponse> {
        common::attach_request_on(
            &self.attach,
            &AttachRequest::Hello {
                client_version: dot_agent_deck::daemon_protocol::PROTOCOL_VERSION,
                client_build_version: None,
            },
        )
        .ok()
    }

    pub(crate) fn inventory(&self) -> AttachResponse {
        common::attach_request_on(&self.attach, &AttachRequest::ListAgents).unwrap()
    }

    pub(crate) fn seed_live(&mut self) {
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
                    remember_command: false,
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
        self.expected_roles = 2;
        assert_eq!(
            self.inventory().orchestration_roles.unwrap().len(),
            self.expected_roles
        );
    }

    pub(crate) fn piped(&self, extra: &[&str]) -> Output {
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

    pub(crate) fn tty(&self, args: &[&str]) -> Terminal {
        Terminal::new(args, &self.env, self.dir.path())
    }

    pub(crate) fn assert_installed(&self) {
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

    pub(crate) fn assert_kept(&mut self) {
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
        assert_eq!(
            inventory.orchestration_roles.unwrap().len(),
            self.expected_roles
        );
        for (_, pid) in &self.agents {
            assert!(common::process_running(*pid), "kept agent {pid} stopped");
        }
    }

    pub(crate) fn assert_restarted(&mut self) {
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
            // SAFETY: only agent PIDs recorded by this fixture.
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

pub(crate) struct Terminal {
    pub(crate) child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    text: Arc<Mutex<Vec<u8>>>,
    grid: Arc<Mutex<vt100::Parser>>,
    drained: Arc<AtomicBool>,
}

impl Terminal {
    pub(crate) fn new(args: &[&str], env: &[(String, String)], cwd: &Path) -> Self {
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

    pub(crate) fn output(&self) -> String {
        String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned()
    }
    pub(crate) fn screen(&self) -> String {
        self.grid.lock().unwrap().screen().contents()
    }
    pub(crate) fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }
    pub(crate) fn wait(&mut self, needle: &str) {
        assert!(
            common::wait_until(WAIT, || self.output().contains(needle)
                || self.screen().contains(needle)),
            "PTY must show {needle:?}; output:\n{}\ngrid:\n{}",
            self.output(),
            self.screen()
        );
    }
    pub(crate) fn success(&mut self) {
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
