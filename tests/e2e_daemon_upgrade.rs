#![cfg(all(feature = "e2e", unix))]

//! Credential-free, headless L2 coverage of the confirmed daemon restart.
//! The installed target is owned by each fixture and atomically replaced;
//! neither the developer's install nor Cargo's executable is overwritten.
//! The successor wrapper records its PID and executes a retained test build.
//! These are process-handover tests, not two-release compatibility or UI tests.

mod common;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery, RestartDaemonRequest};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, CAP_RESTART_DAEMON, RestartAgent, RestartDaemonReply,
    RestartRefusalReason, RestartStopSet, RestartSuccessor,
};
use dot_agent_deck::daemon_restart::{RemoteRestartReport, encode_stop_set_hex};
use spec::spec;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(15);

fn shell_path(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_str()
            .expect("UTF-8 fixture path")
            .replace('\'', "'\\''")
    )
}

fn retain_binary(target: &Path) {
    let built = env!("CARGO_BIN_EXE_dot-agent-deck");
    if fs::hard_link(built, target).is_err() {
        fs::copy(built, target).expect("retain the test binary");
    }
}

struct InstalledDaemon {
    child: Child,
    attach: PathBuf,
    target: PathBuf,
    retained: PathBuf,
    successor_pid: PathBuf,
    agent_pids: Vec<i32>,
    runtime: tokio::runtime::Runtime,
    _dir: TempDir,
}

impl InstalledDaemon {
    fn spawn() -> Self {
        Self::spawn_with(false)
    }

    /// [`Self::spawn`] with the daemon's log in the fixture and its e2e
    /// successor-plan gate armed: after a restart releases the sockets, the
    /// daemon creates `plan-gate/entered` and waits for `plan-gate/release`
    /// before it decides its successor plan.
    fn spawn_gated() -> Self {
        Self::spawn_with(true)
    }

    fn spawn_with(gated: bool) -> Self {
        common::init_test_env();
        let dir = common::harness_tempdir().expect("restart fixture tempdir");
        let home = dir.path().join("home");
        let bin_dir = home.join(".local/bin");
        fs::create_dir_all(&bin_dir).expect("create isolated install directory");
        let target = bin_dir.join("dot-agent-deck");
        let retained = dir.path().join("retained-build");
        retain_binary(&target);
        retain_binary(&retained);
        let attach = dir.path().join("attach.sock");
        let log = fs::File::create(dir.path().join("daemon.log")).expect("daemon log");
        let mut command = Command::new(&target);
        command
            .args(["daemon", "serve"])
            .current_dir(&home)
            .env_clear()
            .env("HOME", &home)
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env("TERM", "xterm-256color")
            .env("DOT_AGENT_DECK_SOCKET", dir.path().join("hook.sock"))
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &attach)
            .env("DOT_AGENT_DECK_STATE_DIR", dir.path().join("state"))
            .env(
                "DOT_AGENT_DECK_SCHEDULES",
                dir.path().join("schedules.toml"),
            )
            .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
            .env("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED", "1")
            .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .process_group(0);
        if gated {
            let gate = dir.path().join("plan-gate");
            fs::create_dir_all(&gate).expect("create the successor-plan gate");
            command
                .env("DOT_AGENT_DECK_E2E_SUCCESSOR_PLAN_GATE", &gate)
                .env("DOT_AGENT_DECK_LOG", dir.path().join("deck.log"));
        }
        let child = command
            .spawn()
            .expect("start real daemon from the owned install path");
        let fixture = Self {
            child,
            attach,
            target,
            retained,
            successor_pid: dir.path().join("successor.pid"),
            agent_pids: Vec::new(),
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("restart client runtime"),
            _dir: dir,
        };
        assert!(
            common::wait_until(WAIT, || fixture.hello().is_some()),
            "the original daemon must answer Hello before the installed file is replaced"
        );
        fixture.install_successor(None);
        fixture
    }

    fn plan_gate(&self, name: &str) -> PathBuf {
        self._dir.path().join("plan-gate").join(name)
    }

    fn deck_log(&self) -> String {
        fs::read_to_string(self._dir.path().join("deck.log")).unwrap_or_default()
    }

    fn wait_for_log(&self, line: &str) {
        assert!(
            common::wait_until(WAIT, || self.deck_log().contains(line)),
            "the daemon never logged {line:?}; log:\n{}",
            self.deck_log()
        );
    }

    fn terminate(&self) {
        // SAFETY: the PID of the daemon this fixture started and still owns.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
    }

    /// Wait for the original daemon to exit, and reap it.
    fn exit_status(&mut self) -> std::process::ExitStatus {
        let exited = {
            let child = RefCell::new(&mut self.child);
            common::wait_until(WAIT, || {
                child
                    .borrow_mut()
                    .try_wait()
                    .expect("poll original daemon")
                    .is_some()
            })
        };
        assert!(exited, "the original daemon never exited");
        self.child.wait().expect("reap original daemon")
    }

    fn hello(&self) -> Option<AttachResponse> {
        self.runtime
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(500),
                    DaemonClient::new(self.attach.clone()).probe_running(),
                )
                .await
            })
            .ok()?
            .ok()?
    }

    fn request(&self, request: &AttachRequest) -> AttachResponse {
        common::attach_request_on(&self.attach, request).expect("attach request")
    }

    fn agents(&self) -> Vec<AgentRecord> {
        let response = self.request(&AttachRequest::ListAgents);
        assert!(response.ok, "ListAgents failed: {:?}", response.error);
        response.agent_records.expect("ListAgents agent inventory")
    }

    fn replace_target(&self, contents: &str, mode: u32) {
        let staged = self.target.with_extension("staged");
        fs::write(&staged, contents).expect("write staged install target");
        fs::set_permissions(&staged, fs::Permissions::from_mode(mode)).expect("target mode");
        fs::rename(staged, &self.target)
            .expect("atomically replace owned target, not linked inode");
    }

    fn install_successor(&self, verification_gate: Option<(&Path, &Path)>) {
        let gate = verification_gate
            .map(|(entered, release)| {
                format!(
                    "printf entered > {}\nwhile [ ! -e {} ]; do sleep 0.02; done\n",
                    shell_path(entered),
                    shell_path(release)
                )
            })
            .unwrap_or_default();
        self.replace_target(&format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then\n{gate}exec {} --version\nfi\nprintf '%s\\n' \"$$\" > {}\nexec {} \"$@\"\n",
            shell_path(&self.retained), shell_path(&self.successor_pid), shell_path(&self.retained)), 0o700);
    }

    fn start_agent(&mut self, label: &str, role_index: Option<usize>) -> AgentRecord {
        let index = self.agent_pids.len();
        let pane = format!("restart-fixture-pane-{index}");
        let pid_file = self._dir.path().join(format!("agent-{index}.pid"));
        let script = self._dir.path().join(format!("agent-{index}.sh"));
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\nexec cat\n",
                shell_path(&pid_file)
            ),
        )
        .expect("write narrow stand-in agent");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("stand-in mode");
        let cwd = self._dir.path().to_str().unwrap().to_owned();
        let response = self.request(&AttachRequest::StartAgent {
            command: Some(format!("/bin/sh {}", shell_path(&script))),
            cwd: Some(cwd.clone()),
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane.clone())],
            display_name: Some(label.into()),
            tab_membership: role_index.map(|role_index| TabMembership::Orchestration {
                name: "restart-team".into(),
                role_index,
                role_name: label.into(),
                is_start_role: role_index == 0,
                orchestration_cwd: Some(cwd),
                display_title: None,
                orchestration_id: Some("restart-instance".into()),
            }),
            agent_type: None,
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        });
        assert!(response.ok, "stand-in spawn failed: {:?}", response.error);
        assert!(
            common::wait_until(WAIT, || read_pid(&pid_file).is_some()),
            "stand-in never started"
        );
        self.agent_pids.push(read_pid(&pid_file).unwrap());
        self.agents()
            .into_iter()
            .find(|agent| agent.pane_id_env.as_deref() == Some(&pane))
            .expect("spawned stand-in must appear in ListAgents")
    }

    fn seed_live_set(&mut self) -> Vec<AgentRecord> {
        vec![
            self.start_agent("ordinary-live", None),
            self.start_agent("lead", Some(0)),
            self.start_agent("coder", Some(1)),
        ]
    }

    fn restart(&self, confirm: Option<RestartStopSet>) -> RestartDaemonReply {
        let result = self
            .runtime
            .block_on(
                DaemonClient::new(self.attach.clone()).restart_daemon(restart_request(confirm)),
            )
            .expect("restart request must yield a structured reply");
        match result {
            GatedQuery::Answered(reply) => reply,
            GatedQuery::Unsupported => panic!("new daemon must advertise {CAP_RESTART_DAEMON}"),
        }
    }

    /// Run this build's `daemon restart-installed --json --confirm-stdin`
    /// against this daemon, as the ssh route does, with
    /// `confirm`'s JSON on its stdin, and parse the report it prints.
    fn restart_installed_via_stdin(&self, confirm: &RestartStopSet) -> RemoteRestartReport {
        use std::io::Write;
        let home = self._dir.path().join("home");
        // The retained build rather than the install target: the target's
        // wrapper records every run but `--version` as the successor.
        let mut child = Command::new(&self.retained)
            .args(["daemon", "restart-installed", "--json", "--confirm-stdin"])
            .current_dir(&home)
            .env_clear()
            .env("HOME", &home)
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env("DOT_AGENT_DECK_SOCKET", self._dir.path().join("hook.sock"))
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &self.attach)
            .env("DOT_AGENT_DECK_STATE_DIR", self._dir.path().join("state"))
            .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "300")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run restart-installed");
        let json = serde_json::to_vec(confirm).unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&json);
        });
        let output = child.wait_with_output().expect("restart-installed output");
        writer.join().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "restart-installed failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"))
    }

    fn assert_untouched(&mut self, expected: &[AgentRecord]) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "refused restart exited original daemon"
        );
        assert!(
            self.hello().is_some(),
            "original daemon stopped answering Hello"
        );
        assert!(!self.successor_pid.exists(), "refusal launched a successor");
        let actual: BTreeSet<_> = self.agents().into_iter().map(|agent| agent.id).collect();
        let wanted: BTreeSet<_> = expected.iter().map(|agent| agent.id.clone()).collect();
        assert_eq!(actual, wanted, "refusal changed live agent identities");
        for pid in &self.agent_pids {
            assert!(
                common::process_running(*pid),
                "refusal stopped stand-in PID {pid}"
            );
        }
        let roles = self
            .request(&AttachRequest::ListAgents)
            .orchestration_roles
            .unwrap_or_default();
        assert_eq!(roles.len(), 2, "refusal lost the orchestration role map");
    }

    fn assert_replaced(&mut self) {
        let old_pid = self.child.id() as i32;
        // Poll the owned Child so an exited process is also reaped on macOS,
        // where a kill(pid, 0) probe cannot distinguish an unreaped zombie.
        let exited = {
            let child = RefCell::new(&mut self.child);
            common::wait_until(WAIT, || {
                child
                    .borrow_mut()
                    .try_wait()
                    .expect("poll original daemon")
                    .is_some()
            })
        };
        assert!(exited, "Accepted must exit the old daemon PID {old_pid}");
        let status = self.child.wait().expect("reap original daemon");
        assert!(
            status.success(),
            "old daemon did not exit cleanly: {status}"
        );
        assert!(
            common::wait_until(WAIT, || read_pid(&self.successor_pid).is_some()),
            "installed target never started a successor"
        );
        let successor_pid = read_pid(&self.successor_pid).unwrap();
        assert_ne!(successor_pid, old_pid, "restart must change the daemon PID");
        assert!(
            common::wait_until(WAIT, || self.hello().is_some()),
            "successor must answer Hello at the same endpoint"
        );
        assert!(
            common::process_running(successor_pid),
            "successor exited after binding"
        );
        assert!(
            self.agents().is_empty(),
            "confirmed agents survived in successor inventory"
        );
        assert!(
            self.request(&AttachRequest::ListAgents)
                .orchestration_roles
                .unwrap_or_default()
                .is_empty(),
            "confirmed role map survived replacement"
        );
        for pid in &self.agent_pids {
            assert!(
                common::wait_until(WAIT, || !common::process_running(*pid)),
                "Accepted left confirmed stand-in PID {pid} alive"
            );
        }
    }
}

fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

impl Drop for InstalledDaemon {
    fn drop(&mut self) {
        // The successor detaches into a new group; the original group's cleanup
        // does not own it. Only fixture-recorded PIDs are signalled here.
        for pid in [Some(self.child.id() as i32), read_pid(&self.successor_pid)]
            .into_iter()
            .flatten()
        {
            if common::process_running(pid) {
                // SAFETY: these are the daemon PIDs recorded by this fixture.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        for pid in &self.agent_pids {
            if common::process_running(*pid) {
                // SAFETY: this fixture's stand-in script recorded its own PID.
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
        }
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "restart fixture daemon log:\n{}",
                fs::read_to_string(self._dir.path().join("daemon.log")).unwrap_or_default()
            );
        }
    }
}

fn restart_request(confirm: Option<RestartStopSet>) -> RestartDaemonRequest {
    RestartDaemonRequest {
        confirm,
        expected_version: None,
        successor: RestartSuccessor::Installed,
    }
}

fn needs_confirmation(reply: RestartDaemonReply, expected_stale: bool) -> RestartStopSet {
    match reply {
        RestartDaemonReply::NeedsConfirmation { at_stake, stale } => {
            assert_eq!(stale, expected_stale, "wrong confirmation freshness");
            at_stake
        }
        other => panic!("expected NeedsConfirmation, got {other:?}"),
    }
}

fn assert_full_set(set: &RestartStopSet, expected: &[AgentRecord]) {
    assert_eq!(
        set.agents.len(),
        expected.len(),
        "disclosure must name every live agent exactly once"
    );
    for record in expected {
        let disclosed = set
            .agents
            .iter()
            .find(|agent| agent.id == record.id)
            .expect("missing live agent");
        assert_eq!(Some(&disclosed.label), record.display_name.as_ref());
        assert_eq!(disclosed.pane_id, record.pane_id_env);
        assert_eq!(disclosed.cwd, record.cwd);
    }
    assert_eq!(set.roles.len(), 2, "disclosure must name both roles");
    for (role, is_orchestrator) in [("lead", true), ("coder", false)] {
        let disclosed = set
            .roles
            .iter()
            .find(|entry| entry.role == role)
            .expect("missing live role");
        assert_eq!(disclosed.orchestration, "restart-team");
        assert_eq!(disclosed.is_orchestrator, is_orchestrator);
        assert!(
            expected
                .iter()
                .any(|agent| agent.pane_id_env.as_deref() == Some(&disclosed.pane_id)),
            "role disclosure must identify the live pane"
        );
    }
}

fn accepted(reply: RestartDaemonReply) -> RestartStopSet {
    match reply {
        RestartDaemonReply::Accepted {
            from_version,
            to_version,
            successor,
            stopping,
        } => {
            assert!(
                !from_version.is_empty(),
                "Accepted must identify original version"
            );
            assert!(
                to_version.is_some(),
                "Installed acceptance must report verified target version"
            );
            assert_eq!(successor, RestartSuccessor::Installed);
            stopping
        }
        other => panic!("expected Accepted, got {other:?}"),
    }
}

/// Scenario: Start an idle daemon from an owned install path, replace that path with a verified successor wrapper, and request restart without confirmation. Accepted must be followed by the old process exiting and a different PID answering Hello on the same endpoint.
#[spec("lifecycle/wire-restart/001")]
#[test]
fn wire_restart_001_idle_daemon_hands_over_to_the_installed_target() {
    let mut daemon = InstalledDaemon::spawn();
    let hello = daemon.hello().unwrap();
    assert!(
        hello
            .capabilities
            .unwrap_or_default()
            .iter()
            .any(|cap| cap == CAP_RESTART_DAEMON)
    );
    let stopping = accepted(daemon.restart(None));
    assert!(stopping.agents.is_empty() && stopping.roles.is_empty());
    daemon.assert_replaced();
}

/// Scenario: Start one ordinary stand-in and two orchestration roles, then ask the daemon to restart without confirmation twice. Both replies must disclose every agent and role with stale false, while the original process, agents and role map remain alive.
#[spec("lifecycle/wire-restart/002")]
#[test]
fn wire_restart_002_live_agents_and_roles_require_confirmation_without_stopping() {
    let mut daemon = InstalledDaemon::spawn();
    let agents = daemon.seed_live_set();
    for _ in [0, 1] {
        let set = needs_confirmation(daemon.restart(None), false);
        assert_full_set(&set, &agents);
        daemon.assert_untouched(&agents);
    }
}

/// Scenario: Ask for a restart with live stand-ins and roles, then return the disclosed confirmation set in reverse order. The daemon must accept, stop the named processes and roles, and serve the same endpoint from a successor PID.
#[spec("lifecycle/wire-restart/003")]
#[test]
fn wire_restart_003_matching_confirmation_stops_named_agents_and_replaces_daemon() {
    let mut daemon = InstalledDaemon::spawn();
    let agents = daemon.seed_live_set();
    let mut confirm = needs_confirmation(daemon.restart(None), false);
    assert_full_set(&confirm, &agents);
    confirm.agents.reverse();
    confirm.roles.reverse();
    let stopping = accepted(daemon.restart(Some(confirm)));
    assert_full_set(&stopping, &agents);
    daemon.assert_replaced();
}

/// Scenario: Obtain a confirmation set, then start another stand-in with the same display name as an existing one before returning the old set. The daemon must report stale true with the new full set, preserve every process and role, and keep its original PID.
#[spec("lifecycle/wire-restart/004")]
#[test]
fn wire_restart_004_stale_confirmation_discloses_new_full_set_without_stopping() {
    let mut daemon = InstalledDaemon::spawn();
    let mut agents = daemon.seed_live_set();
    let confirm = needs_confirmation(daemon.restart(None), false);
    assert_full_set(&confirm, &agents);
    agents.push(daemon.start_agent("ordinary-live", None));
    let fresh = needs_confirmation(daemon.restart(Some(confirm)), true);
    assert_full_set(&fresh, &agents);
    daemon.assert_untouched(&agents);
}

/// Scenario: With stand-in agents and roles alive, replace the daemon's install target with a non-executable file and then an executable whose version probe fails. Each request must refuse with the appropriate target reason and leave the original daemon, agents and roles untouched.
#[spec("lifecycle/wire-restart/005")]
#[test]
fn wire_restart_005_unverified_install_target_preserves_daemon_and_live_agents() {
    let mut daemon = InstalledDaemon::spawn();
    let agents = daemon.seed_live_set();
    for (script, mode, expected) in [
        (
            "#!/bin/sh\nexit 0\n",
            0o600,
            RestartRefusalReason::TargetMissing,
        ),
        (
            "#!/bin/sh\nexit 23\n",
            0o700,
            RestartRefusalReason::TargetDidNotAnswer,
        ),
    ] {
        daemon.replace_target(script, mode);
        match daemon.restart(None) {
            RestartDaemonReply::Refused { reason, message } => {
                assert_eq!(reason, expected);
                assert!(!message.is_empty(), "target refusal must explain why");
            }
            other => panic!("unverified target must refuse before asking consent: {other:?}"),
        }
        daemon.assert_untouched(&agents);
    }
}

/// Scenario: Hold the first restart inside a test-controlled version probe, then send a second request while verification is in progress. Exactly the first request must be accepted and the second must return InProgress before the probe is released, followed by a working successor.
#[spec("lifecycle/wire-restart/006")]
#[test]
fn wire_restart_006_concurrent_requests_accept_exactly_one_restart() {
    let mut daemon = InstalledDaemon::spawn();
    let entered = daemon._dir.path().join("verification-entered");
    let release = daemon._dir.path().join("verification-release");
    daemon.install_successor(Some((&entered, &release)));
    let attach = daemon.attach.clone();
    let first = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(DaemonClient::new(attach).restart_daemon(restart_request(None)))
    });
    assert!(
        common::wait_until(WAIT, || entered.exists()),
        "first request never began target verification"
    );
    let client = DaemonClient::new(daemon.attach.clone());
    let second = daemon.runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(2),
            client.restart_daemon(restart_request(None)),
        )
        .await
    });
    // Always unblock the first verifier before asserting the second result.
    fs::write(&release, "release").expect("release first version probe");
    match second
        .expect("concurrent restart must refuse immediately")
        .expect("second reply")
    {
        GatedQuery::Answered(RestartDaemonReply::Refused { reason, .. }) => {
            assert_eq!(reason, RestartRefusalReason::InProgress);
        }
        other => panic!("second request must refuse InProgress, got {other:?}"),
    }
    match first
        .join()
        .expect("first requester panicked")
        .expect("first reply")
    {
        GatedQuery::Answered(reply) => assert!(accepted(reply).is_empty()),
        other => panic!("first request was not answered: {other:?}"),
    }
    daemon.assert_replaced();
}

/// Scenario: Start an idle daemon from an owned install path with its
/// successor-plan gate armed, and request a restart onto a verified installed
/// build. Once it is accepted and the daemon has released its sockets and
/// stopped at the gate — before it decides whether a successor follows —
/// send it SIGTERM, then open the gate. The signal is a stop that wins: the
/// daemon exits cleanly and no successor is ever started (PRD #1487 audit A2).
#[spec("lifecycle/wire-restart/007")]
#[test]
fn wire_restart_007_a_signal_before_the_successor_decision_starts_no_successor() {
    let mut daemon = InstalledDaemon::spawn_gated();
    assert!(accepted(daemon.restart(None)).is_empty());
    assert!(
        common::wait_until(WAIT, || daemon.plan_gate("entered").exists()),
        "the daemon never reached its successor decision"
    );
    daemon.terminate();
    daemon.wait_for_log("a stop arrived after a restart was accepted");
    fs::write(daemon.plan_gate("release"), "release").expect("open the plan gate");
    let status = daemon.exit_status();
    assert!(
        status.success(),
        "the stopped daemon exits cleanly: {status}"
    );
    // A spawned successor records its PID at once; give it the chance.
    assert!(
        !common::wait_until(Duration::from_secs(2), || daemon.successor_pid.exists()),
        "a successor started after the signal asked the daemon to stop"
    );
    assert!(daemon.hello().is_none(), "nothing answers at the endpoint");
}

/// Scenario: Start an idle daemon with its successor-plan gate armed, request
/// a restart and let it be accepted; at the gate, replace the installed build
/// with one whose version check blocks, then open the gate so the daemon
/// commits to the restart and stalls re-verifying that build. A first SIGTERM
/// is logged and not acted on — the daemon is exiting into its successor — and
/// a second one force-exits it at once with status 143, before any successor
/// starts (PRD #1487 audit A2).
#[spec("lifecycle/wire-restart/008")]
#[test]
fn wire_restart_008_a_second_signal_during_successor_verification_exits() {
    let mut daemon = InstalledDaemon::spawn_gated();
    assert!(accepted(daemon.restart(None)).is_empty());
    assert!(
        common::wait_until(WAIT, || daemon.plan_gate("entered").exists()),
        "the daemon never reached its successor decision"
    );
    let entered = daemon._dir.path().join("recheck-entered");
    let release = daemon._dir.path().join("recheck-release");
    daemon.install_successor(Some((&entered, &release)));
    fs::write(daemon.plan_gate("release"), "release").expect("open the plan gate");
    assert!(
        common::wait_until(WAIT, || entered.exists()),
        "the daemon never re-verified the replaced build"
    );
    daemon.terminate();
    daemon.wait_for_log("termination signal after this daemon committed to its restart");
    daemon.terminate();
    let status = daemon.exit_status();
    // Free the blocked version check whatever happened above.
    fs::write(&release, "release").expect("release the version check");
    assert_eq!(
        status.code(),
        Some(143),
        "the second signal force-exits: {status}"
    );
    assert!(
        !common::wait_until(Duration::from_secs(2), || daemon.successor_pid.exists()),
        "no successor starts after the forced exit"
    );
}

/// Scenario: With stand-in agents and roles alive, run `daemon restart-installed --confirm-stdin` and write a confirmation to its stdin that names 600 agents, too large for the 128 KiB command-line limit once hex-encoded. The daemon reads the whole set and answers that it is stale, disclosing the real three, and nothing stops. Sending the disclosed set the same way restarts the daemon (issue #1619).
#[spec("lifecycle/wire-restart/009")]
#[test]
fn wire_restart_009_restart_installed_reads_a_confirmation_too_large_for_the_command_line() {
    let mut daemon = InstalledDaemon::spawn();
    let agents = daemon.seed_live_set();
    let oversized = RestartStopSet {
        agents: (0..600)
            .map(|i| RestartAgent {
                id: format!("agent-{i:04}-0123456789abcdef"),
                label: format!("claude: refactor the module at index {i}"),
                pane_id: Some(format!("pane-{i}")),
                cwd: Some(format!("/home/someone/work/repository-{i}/worktree")),
            })
            .collect(),
        roles: Vec::new(),
    };
    assert!(encode_stop_set_hex(&oversized).len() > 128 * 1024);

    let report = daemon.restart_installed_via_stdin(&oversized);
    assert!(report.running && !report.unsupported, "{report:?}");
    let fresh = needs_confirmation(report.reply.expect("the daemon answered"), true);
    assert_full_set(&fresh, &agents);
    daemon.assert_untouched(&agents);

    let report = daemon.restart_installed_via_stdin(&fresh);
    let stopping = accepted(report.reply.expect("the daemon answered"));
    assert_full_set(&stopping, &agents);
    daemon.assert_replaced();
}

/// Stands in for systemd running the unit `daemon install` writes:
/// `Restart=on-failure`, so a non-zero exit runs `ExecStart` again and a clean
/// exit ends the service. Each run gets `INVOCATION_ID` and a
/// `SYSTEMD_EXEC_PID` naming the daemon's own pid, which is what the daemon
/// reads as "systemd started me". The first run can carry a build-id override,
/// the older build the service ran before the upgrade; later runs are the
/// build installed at the unit's path. Arguments: binary, state directory,
/// first run's build id (may be empty).
const SUPERVISOR: &str = r#"#!/bin/sh
bin=$1; state=$2; first_build=$3
runs=0
while [ "$runs" -lt 3 ]; do
  runs=$((runs + 1))
  if [ "$runs" -eq 1 ] && [ -n "$first_build" ]; then
    DOT_AGENT_DECK_BUILD_ID_OVERRIDE=$first_build; export DOT_AGENT_DECK_BUILD_ID_OVERRIDE
  else
    unset DOT_AGENT_DECK_BUILD_ID_OVERRIDE
  fi
  INVOCATION_ID=dad-e2e-supervised sh -c 'SYSTEMD_EXEC_PID=$$; export SYSTEMD_EXEC_PID; exec "$0" daemon serve' "$bin" &
  printf '%s\n' "$!" > "$state/service.pid.tmp"
  mv "$state/service.pid.tmp" "$state/service.pid"
  wait "$!"
  status=$?
  printf '%s\n' "$status" >> "$state/exits"
  [ "$status" -eq 0 ] && exit 0
done
exit 1
"#;

/// A real `daemon serve` under [`SUPERVISOR`], issue #1760.
struct SupervisedDaemon {
    supervisor: Child,
    binary: PathBuf,
    attach: PathBuf,
    runtime: tokio::runtime::Runtime,
    dir: TempDir,
}

impl SupervisedDaemon {
    fn spawn(first_build: Option<&str>) -> Self {
        common::init_test_env();
        let dir = common::harness_tempdir().expect("supervised fixture tempdir");
        fs::create_dir_all(dir.path().join("home")).expect("create isolated home");
        let binary = dir.path().join("installed-build");
        retain_binary(&binary);
        let script = dir.path().join("supervisor.sh");
        fs::write(&script, SUPERVISOR).expect("write the stand-in supervisor");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("supervisor mode");
        let log = fs::File::create(dir.path().join("daemon.log")).expect("daemon log");
        let attach = dir.path().join("attach.sock");
        let mut command = Command::new(&script);
        command
            .arg(&binary)
            .arg(dir.path())
            .arg(first_build.unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .process_group(0);
        Self::daemon_env(&mut command, dir.path(), &attach);
        let supervisor = command.spawn().expect("start the stand-in supervisor");
        let fixture = Self {
            supervisor,
            binary,
            attach,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("supervised client runtime"),
            dir,
        };
        assert!(
            common::wait_until(WAIT, || fixture.hello().is_some()),
            "the supervised daemon never answered Hello"
        );
        fixture
    }

    /// What every daemon of this fixture runs with, the service's or a
    /// client's: the same isolated endpoints, state and home.
    fn daemon_env(command: &mut Command, dir: &Path, attach: &Path) {
        let home = dir.join("home");
        command
            .current_dir(&home)
            .env_clear()
            .env("HOME", &home)
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env("TERM", "xterm-256color")
            .env("DOT_AGENT_DECK_SOCKET", dir.join("hook.sock"))
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", attach)
            .env("DOT_AGENT_DECK_STATE_DIR", dir.join("state"))
            .env("DOT_AGENT_DECK_SCHEDULES", dir.join("schedules.toml"))
            .env("DOT_AGENT_DECK_LOG", dir.join("deck.log"))
            .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
            .env("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED", "1")
            .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "300");
    }

    fn hello(&self) -> Option<AttachResponse> {
        self.runtime
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(500),
                    DaemonClient::new(self.attach.clone()).probe_running(),
                )
                .await
            })
            .ok()?
            .ok()?
    }

    /// The pid of the daemon the service runs now.
    fn service_pid(&self) -> i32 {
        read_pid(&self.dir.path().join("service.pid")).expect("the supervisor records its daemon")
    }

    /// The pid of whatever daemon is listening at the endpoint.
    fn serving_pid(&self) -> Option<i32> {
        self.runtime.block_on(async {
            let stream = dot_agent_deck::platform::ipc::IpcStream::connect(&self.attach)
                .await
                .ok()?;
            dot_agent_deck::platform::peercred::peer_pid(&stream)
                .ok()
                .map(|pid| pid as i32)
        })
    }

    fn exits(&self) -> String {
        fs::read_to_string(self.dir.path().join("exits")).unwrap_or_default()
    }

    /// The local client's own daemon, unsupervised: what the desktop's
    /// `spawn_local_daemon` starts, standing in for it here. Records its pid in
    /// `client-spawned.pid`.
    fn client_spawn(&self) -> Box<dyn Fn() -> Result<(), String> + Send + Sync> {
        let binary = self.binary.clone();
        let dir = self.dir.path().to_path_buf();
        let attach = self.attach.clone();
        Box::new(move || {
            let mut command = Command::new(&binary);
            command
                .args(["daemon", "serve"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            Self::daemon_env(&mut command, &dir, &attach);
            let child = command.spawn().map_err(|e| e.to_string())?;
            fs::write(dir.join("client-spawned.pid"), child.id().to_string())
                .map_err(|e| e.to_string())
        })
    }

    fn client_spawned(&self) -> Option<i32> {
        read_pid(&self.dir.path().join("client-spawned.pid"))
    }

    /// What the user sees with `systemctl --user status`: the service is
    /// still active, its daemon is a new process the service started after a
    /// failure-status exit, and that daemon is the one serving the endpoint.
    fn assert_back_under_its_service(&mut self, old_pid: i32) {
        assert!(
            common::wait_until(WAIT, || self
                .serving_pid()
                .is_some_and(|pid| pid != old_pid)),
            "no new daemon is serving the endpoint"
        );
        assert!(
            self.supervisor
                .try_wait()
                .expect("poll supervisor")
                .is_none(),
            "the service stopped: its daemon exited with {:?}, which Restart=on-failure does \
             not restart, and the client's own daemon (pid {:?}) is serving in its place",
            self.exits().trim(),
            self.client_spawned()
        );
        let service_pid = self.service_pid();
        assert_ne!(
            service_pid, old_pid,
            "the service never restarted its daemon"
        );
        assert_eq!(
            self.serving_pid(),
            Some(service_pid),
            "the daemon serving the endpoint is not the one the service runs"
        );
        assert_eq!(
            self.exits().trim(),
            dot_agent_deck::daemon_restart::SUPERVISED_RESTART_EXIT.to_string(),
            "the old daemon must hand back to the service with the failure status"
        );
        assert_eq!(
            self.client_spawned(),
            None,
            "the client started an unsupervised daemon beside the service"
        );
    }
}

impl Drop for SupervisedDaemon {
    fn drop(&mut self) {
        let group = self.supervisor.id() as i32;
        let service = read_pid(&self.dir.path().join("service.pid"));
        for pid in [Some(group), service, self.client_spawned()]
            .into_iter()
            .flatten()
        {
            if common::process_running(pid) {
                // SAFETY: the supervisor's group and the daemon pids this
                // fixture recorded.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        let _ = self.supervisor.wait();
        if std::thread::panicking() {
            eprintln!(
                "supervised fixture log:\n{}\nexits: {}",
                fs::read_to_string(self.dir.path().join("daemon.log")).unwrap_or_default(),
                self.exits()
            );
        }
    }
}

/// Scenario: Run an idle daemon under a stand-in for the `daemon install`
/// service (systemd's `Restart=on-failure`) and replace it from this machine
/// the way the desktop's Replace does. The service must still be running
/// afterwards, with a new daemon it started serving the endpoint, and the
/// client must not have started a daemon of its own (issue #1760).
#[spec("lifecycle/wire-restart/010")]
#[test]
fn wire_restart_010_a_local_replace_hands_a_supervised_daemon_back_to_its_service() {
    use dot_agent_deck::daemon_client::{Endpoint, LocalEndpoint};
    use dot_agent_deck::daemon_upgrade::{
        CLIENT_VERSION, NoDecider, NoInstall, UpgradeOutcome, UpgradePlan, WireDaemonPort,
        upgrade_daemon,
    };
    let mut daemon = SupervisedDaemon::spawn(None);
    let old_pid = daemon.service_pid();
    let port = WireDaemonPort::new(
        &Endpoint::Local(LocalEndpoint::at(daemon.attach.clone())),
        daemon.runtime.handle().clone(),
        daemon.client_spawn(),
    )
    .expect("a port for the local daemon");
    let plan = UpgradePlan {
        version: CLIENT_VERSION.into(),
        successor: RestartSuccessor::ClientSpawns,
    };
    let outcome = upgrade_daemon(
        "this machine",
        &plan,
        &NoInstall,
        &port,
        &NoDecider,
        &mut |_| {},
    );
    assert!(
        matches!(outcome, UpgradeOutcome::Restarted { .. }),
        "{outcome:?}"
    );
    daemon.assert_back_under_its_service(old_pid);
}

/// Scenario: The control for 010: the same supervised daemon asked for the
/// remote upgrade's restart onto its installed build hands back to the service,
/// which starts the daemon again. It passes without the fix, so the stand-in
/// supervisor and the daemon's detection of it are not what 010 catches.
#[spec("lifecycle/wire-restart/010")]
#[test]
fn wire_restart_010_control_an_installed_restart_already_hands_back() {
    let mut daemon = SupervisedDaemon::spawn(None);
    let old_pid = daemon.service_pid();
    let reply = daemon
        .runtime
        .block_on(
            DaemonClient::new(daemon.attach.clone()).restart_daemon(RestartDaemonRequest {
                confirm: None,
                expected_version: Some(dot_agent_deck::daemon_upgrade::CLIENT_VERSION.into()),
                successor: RestartSuccessor::Installed,
            }),
        )
        .expect("restart request must yield a structured reply");
    assert!(
        matches!(
            reply,
            GatedQuery::Answered(RestartDaemonReply::Accepted { .. })
        ),
        "{reply:?}"
    );
    daemon.assert_back_under_its_service(old_pid);
}

/// Scenario: Run an idle daemon of an older build under a stand-in for the
/// `daemon install` service, then launch this build's TUI against it. The
/// launch-time version check restarts the daemon and the dashboard appears;
/// the service must still be running, now on this build, with its own daemon
/// serving the endpoint the TUI attached to (issue #1760).
#[spec("lifecycle/wire-restart/011")]
#[test]
fn wire_restart_011_the_tui_version_check_hands_a_supervised_daemon_back_to_its_service() {
    let mut daemon = SupervisedDaemon::spawn(Some("0.0.1-g0000old"));
    let old_pid = daemon.service_pid();
    assert_eq!(
        daemon.hello().and_then(|h| h.build_version).as_deref(),
        Some("0.0.1-g0000old")
    );
    let deck = common::TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon
                .dir
                .path()
                .join("hook.sock")
                .to_string_lossy()
                .to_string(),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    daemon.assert_back_under_its_service(old_pid);
    assert_eq!(
        daemon.hello().and_then(|h| h.build_version),
        Some(dot_agent_deck::build_id::local_build_id()),
        "the service's daemon must be this build"
    );
}
