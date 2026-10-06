//! Reach a remote machine's daemon over ssh, through that machine's own deck
//! binary (PRD #1487).
//!
//! The laptop does not talk to a remote daemon's socket here. It runs the deck
//! CLI on the remote — `<binary> daemon probe --json`, `<binary> daemon
//! restart-installed --json …` — and parses the one JSON line each prints. That
//! keeps every remote operation on the one route the deck list records (the
//! same [`SshTarget`], jump host included) and needs no tunnel.
//!
//! Deliberately general: nothing here knows about upgrades. It is "run this
//! deck's CLI on the remote against that machine's daemon and parse a JSON
//! line", and issue #1490 adds `start_daemon()` beside [`SshDaemonPort::probe`]
//! and [`SshDaemonPort::restart_installed`]. The upgrade flow adapts it to its
//! own port trait in [`crate::daemon_upgrade`].

use std::cell::RefCell;
use std::time::Duration;

use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::daemon_protocol::RestartStopSet;
use crate::daemon_restart::{DaemonProbe, RemoteRestartReport, encode_stop_set_hex};
use crate::remote::{RemoteEntry, SshError, SshExecutor, SshTarget};

/// Upper bound on what one plumbing command may print. A `Hello` reply with a
/// full capability list and a running-agents summary is a few KiB; anything
/// near this is not a reply this build wrote.
pub const REMOTE_DAEMON_REPLY_CAP: usize = 256 * 1024;

/// What one plumbing command may spend before the remote binary has anything
/// to say: ssh's own connect phase (the upgrade executor's `ConnectTimeout`),
/// then the remote binary starting and doing its own bounded handshake with
/// the daemon there.
const PLUMBING_START_ALLOWANCE: Duration =
    Duration::from_secs(crate::daemon_upgrade::UPGRADE_SSH_CONNECT_TIMEOUT + 15);

/// Laptop-side wall-clock bound on one `daemon probe --json` (PRD #1487 audit
/// A1). The remote side is a single bounded `Hello`, so nothing but the start
/// allowance is owed.
pub const REMOTE_PROBE_DEADLINE: Duration = PLUMBING_START_ALLOWANCE;

/// Laptop-side wall-clock bound on one `daemon restart-installed --json`: the
/// start allowance plus the whole restart round trip the remote client itself
/// allows ([`crate::daemon_client::RESTART_REQUEST_TIMEOUT`]), which already
/// covers the daemon checking the installed build
/// ([`crate::daemon_restart::RESTART_VERIFY_TIMEOUT`]). The drain happens after
/// the daemon has answered, so it costs this command nothing.
pub const REMOTE_RESTART_DEADLINE: Duration =
    PLUMBING_START_ALLOWANCE.saturating_add(crate::daemon_client::RESTART_REQUEST_TIMEOUT);

// The restart round trip must leave room for the check inside it.
const _: () = assert!(
    crate::daemon_client::RESTART_REQUEST_TIMEOUT.as_secs()
        > crate::daemon_restart::RESTART_VERIFY_TIMEOUT.as_secs()
);

/// The exit code clap uses for a usage error — what a deck binary that predates
/// a plumbing subcommand exits with when asked to run it.
const CLAP_USAGE_EXIT: i32 = 2;

/// Why a remote daemon operation produced no answer.
#[derive(Debug, Error)]
pub enum RemoteDaemonError {
    /// ssh itself failed (unreachable, authentication, host key).
    #[error(transparent)]
    Ssh(#[from] SshError),
    /// The remote binary does not know the subcommand: it is older than the
    /// operation.
    #[error("the deck binary on the remote is too old for this operation: {stderr}")]
    Unsupported { stderr: String },
    /// The subcommand ran and failed.
    #[error("the remote command failed (exit {status}): {stderr}")]
    Failed { status: i32, stderr: String },
    /// The subcommand exited 0 but printed nothing this build can read.
    #[error("the remote command printed an unreadable reply: {0}")]
    Malformed(String),
}

/// A remote machine's daemon, reached through the deck binary installed there.
///
/// `binary` is spelled for the remote shell (it may start with `~`), exactly as
/// [`RemoteEntry::remote_binary`] returns it. It can be repointed after an
/// install moved the deck (a Homebrew install replacing `~/.local/bin`), which
/// is why it sits in a `RefCell`.
pub struct SshDaemonPort<E: SshExecutor> {
    executor: E,
    target: SshTarget,
    binary: RefCell<String>,
    probe_deadline: Duration,
    restart_deadline: Duration,
}

impl<E: SshExecutor> SshDaemonPort<E> {
    pub fn new(executor: E, target: SshTarget, binary: impl Into<String>) -> Self {
        Self {
            executor,
            target,
            binary: RefCell::new(binary.into()),
            probe_deadline: REMOTE_PROBE_DEADLINE,
            restart_deadline: REMOTE_RESTART_DEADLINE,
        }
    }

    /// Bound each probe and each restart request by these wall-clock deadlines
    /// instead of [`REMOTE_PROBE_DEADLINE`] / [`REMOTE_RESTART_DEADLINE`].
    pub fn with_deadlines(mut self, probe: Duration, restart: Duration) -> Self {
        self.probe_deadline = probe;
        self.restart_deadline = restart;
        self
    }

    /// The port for a deck-list row: its ssh target (jump host included) and
    /// its recorded binary.
    pub fn for_entry(executor: E, entry: &RemoteEntry) -> Self {
        Self::new(executor, entry.ssh_target(), entry.remote_binary())
    }

    /// Run later commands through `binary` instead.
    pub fn set_binary(&self, binary: impl Into<String>) {
        *self.binary.borrow_mut() = binary.into();
    }

    /// The binary later commands run, as spelled for the remote shell.
    pub fn binary(&self) -> String {
        self.binary.borrow().clone()
    }

    /// The ssh target every command goes to.
    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    /// `daemon probe --json` on the remote: whether a daemon runs at that
    /// machine's endpoint, and its `Hello` reply. Never starts one.
    pub fn probe(&self) -> Result<DaemonProbe, RemoteDaemonError> {
        self.run_json("daemon probe --json", self.probe_deadline)
    }

    /// `daemon restart-installed --json` on the remote: ask that machine's
    /// daemon to restart onto the build installed at its own path.
    /// `expected_version` and `confirm` are passed through to the daemon.
    pub fn restart_installed(
        &self,
        expected_version: Option<&str>,
        confirm: Option<&RestartStopSet>,
    ) -> Result<RemoteRestartReport, RemoteDaemonError> {
        let mut args = String::from("daemon restart-installed --json");
        if let Some(version) = expected_version {
            args.push_str(" --expect-version ");
            args.push_str(&crate::remote::shell_word(version));
        }
        if let Some(set) = confirm {
            // Hex: no shell metacharacter can appear in it.
            args.push_str(" --confirm-hex ");
            args.push_str(&encode_stop_set_hex(set));
        }
        self.run_json(&args, self.restart_deadline)
    }

    /// Run `<binary> <args>` and parse the last non-empty stdout line as `T`.
    ///
    /// Bounded however the executor was built (audit A1): each stream is
    /// capped at [`REMOTE_DAEMON_REPLY_CAP`] while it drains and the session is
    /// killed at `deadline`, through [`SshExecutor::run_capped_within`] — so a
    /// remote that streams without end, or never finishes, costs a bounded
    /// amount of memory and time even on the keepalive-only upgrade executor.
    fn run_json<T: DeserializeOwned>(
        &self,
        args: &str,
        deadline: Duration,
    ) -> Result<T, RemoteDaemonError> {
        // The binary is a `RemoteBinaryPath` or the `~/.local/bin` constant,
        // both free of shell metacharacters, and left unquoted so the remote
        // shell expands `~` (see `RemoteEntry::remote_binary`).
        let command = format!("{} {args}", self.binary.borrow());
        let capped = self.executor.run_capped_within(
            &self.target,
            &command,
            REMOTE_DAEMON_REPLY_CAP,
            deadline,
        )?;
        let output = capped.output;
        // A stream that reached the cap is not a reply this build wrote,
        // whatever the exit status — and a remote that kept writing past it
        // usually dies of the closed pipe, so its status says nothing useful.
        if capped.truncated {
            return Err(RemoteDaemonError::Malformed(format!(
                "more than {REMOTE_DAEMON_REPLY_CAP} bytes"
            )));
        }
        // Remote-controlled text: scrubbed before it can reach a terminal.
        let stderr = crate::remote::scrub_remote_text(output.stderr.trim());
        if output.status == CLAP_USAGE_EXIT {
            return Err(RemoteDaemonError::Unsupported { stderr });
        }
        if output.status != 0 {
            return Err(RemoteDaemonError::Failed {
                status: output.status,
                stderr,
            });
        }
        let line = output
            .stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or_else(|| RemoteDaemonError::Malformed("no output".into()))?;
        serde_json::from_str(line).map_err(|e| RemoteDaemonError::Malformed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::{AttachResponse, RestartAgent, RestartDaemonReply};
    use crate::remote::SshOutput;

    /// Records each command and answers from a script.
    struct Scripted {
        commands: RefCell<Vec<String>>,
        reply: SshOutput,
    }

    impl SshExecutor for Scripted {
        fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
            self.commands.borrow_mut().push(command.to_string());
            Ok(self.reply.clone())
        }
    }

    fn port(status: i32, stdout: &str, stderr: &str) -> SshDaemonPort<Scripted> {
        SshDaemonPort::new(
            Scripted {
                commands: RefCell::new(Vec::new()),
                reply: SshOutput {
                    status,
                    stdout: stdout.to_string(),
                    stderr: stderr.to_string(),
                },
            },
            SshTarget::parse("u@h", 22, None),
            "~/.local/bin/dot-agent-deck",
        )
    }

    #[test]
    fn probe_parses_the_last_json_line() {
        let hello = AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION);
        let line = serde_json::to_string(&DaemonProbe {
            running: true,
            hello: Some(hello.clone()),
        })
        .unwrap();
        let p = port(0, &format!("noise\n{line}\n\n"), "");
        let probe = p.probe().unwrap();
        assert!(probe.running);
        assert_eq!(
            probe.hello.and_then(|h| h.server_version),
            hello.server_version
        );
        assert_eq!(
            p.executor.commands.borrow().as_slice(),
            ["~/.local/bin/dot-agent-deck daemon probe --json"]
        );
    }

    #[test]
    fn a_usage_error_is_unsupported_and_other_failures_are_failed() {
        let p = port(2, "", "error: unrecognized subcommand 'probe'");
        assert!(matches!(
            p.probe(),
            Err(RemoteDaemonError::Unsupported { .. })
        ));
        let p = port(1, "", "daemon probe: no handshake within 5s");
        assert!(matches!(
            p.probe(),
            Err(RemoteDaemonError::Failed { status: 1, .. })
        ));
        let p = port(0, "not json", "");
        assert!(matches!(p.probe(), Err(RemoteDaemonError::Malformed(_))));
        let p = port(0, "", "");
        assert!(matches!(p.probe(), Err(RemoteDaemonError::Malformed(_))));
    }

    #[test]
    fn restart_installed_passes_version_and_hex_confirmation() {
        let report = RemoteRestartReport {
            running: true,
            reply: Some(RestartDaemonReply::NeedsConfirmation {
                at_stake: RestartStopSet::default(),
                stale: false,
            }),
            unsupported: false,
        };
        let p = port(0, &serde_json::to_string(&report).unwrap(), "");
        p.set_binary("/opt/homebrew/bin/dot-agent-deck");
        let set = RestartStopSet {
            agents: vec![RestartAgent {
                id: "a1".into(),
                label: "it's; rm -rf ~".into(),
                pane_id: None,
                cwd: None,
            }],
            roles: vec![],
        };
        assert_eq!(
            p.restart_installed(Some("0.46.0"), Some(&set)).unwrap(),
            report
        );
        let commands = p.executor.commands.borrow();
        assert_eq!(
            commands[0],
            format!(
                "/opt/homebrew/bin/dot-agent-deck daemon restart-installed --json --expect-version 0.46.0 --confirm-hex {}",
                encode_stop_set_hex(&set)
            )
        );
        assert!(!commands[0].contains(';') && !commands[0].contains('\''));
    }

    /// PRD #1487 audit A1: the production executor shape — the upgrade path's
    /// keepalive-only [`crate::daemon_upgrade::upgrade_ssh_executor`], which
    /// imposes no wall-clock kill of its own — with `ssh` swapped for a stand-in
    /// that behaves like a hostile or broken remote. Most stand-ins `exec` one
    /// process, as `ssh` itself is one process; the descendant cases start a
    /// second that inherits the streams, as a `ProxyCommand` does.
    #[cfg(unix)]
    mod production_executor_bounds {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::time::Instant;

        const DEADLINE: Duration = Duration::from_secs(2);
        /// Far above `DEADLINE`, far below "forever": an unbounded run fails
        /// the test instead of hanging the tier.
        const MUST_RETURN_WITHIN: Duration = Duration::from_secs(30);

        fn port_running(
            dir: &std::path::Path,
            body: &str,
        ) -> SshDaemonPort<crate::remote::SystemSshExecutor> {
            let script = dir.join("ssh");
            crate::test_isolation::write_script(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            SshDaemonPort::new(
                crate::daemon_upgrade::upgrade_ssh_executor().with_program(&script),
                SshTarget::parse("u@h", 22, None),
                "~/.local/bin/dot-agent-deck",
            )
            .with_deadlines(DEADLINE, DEADLINE)
        }

        /// A flood is stopped at the cap: the drainer closes the pipe, the
        /// stand-in dies of it, and the reply is refused as over the cap —
        /// long before the deadline.
        fn assert_refused_at_cap<T: std::fmt::Debug>(
            started: Instant,
            result: Result<T, RemoteDaemonError>,
        ) {
            let elapsed = started.elapsed();
            assert!(
                elapsed < MUST_RETURN_WITHIN,
                "the command was not bounded: {elapsed:?}"
            );
            match result {
                Err(RemoteDaemonError::Malformed(why)) => assert!(
                    why.contains(&format!("more than {REMOTE_DAEMON_REPLY_CAP} bytes")),
                    "unexpected reason: {why}"
                ),
                other => panic!("expected the cap to refuse the flood, got {other:?}"),
            }
        }

        fn assert_stopped_at_deadline<T: std::fmt::Debug>(
            started: Instant,
            result: Result<T, RemoteDaemonError>,
        ) {
            let elapsed = started.elapsed();
            assert!(
                elapsed < MUST_RETURN_WITHIN,
                "the command was not bounded: {elapsed:?}"
            );
            match result {
                Err(RemoteDaemonError::Ssh(SshError::Other { detail, .. })) => assert!(
                    detail.contains("did not finish within 2s"),
                    "unexpected detail: {detail}"
                ),
                other => panic!("expected the deadline to stop the command, got {other:?}"),
            }
        }

        /// Scenario: the remote command prints without end on stdout. The
        /// drainer stops at the reply cap and closes the pipe, so the stream is
        /// never accumulated, and the reply is refused as over the cap.
        #[test]
        fn an_endless_stdout_reply_is_capped_and_stopped() {
            let dir = crate::test_temp::tempdir().unwrap();
            let p = port_running(dir.path(), "exec yes");
            let started = Instant::now();
            assert_refused_at_cap(started, p.probe());
        }

        /// Scenario: the same flood on stderr, which `Command::output()` would
        /// have collected for as long as the remote kept writing.
        #[test]
        fn an_endless_stderr_reply_is_capped_and_stopped() {
            let dir = crate::test_temp::tempdir().unwrap();
            let p = port_running(dir.path(), "exec yes >&2");
            let started = Instant::now();
            assert_refused_at_cap(started, p.restart_installed(None, None));
        }

        /// Scenario: a remote command that never finishes and prints nothing,
        /// over a transport that stays alive — keepalives cannot see it.
        #[test]
        fn a_remote_command_that_never_finishes_is_stopped() {
            let dir = crate::test_temp::tempdir().unwrap();
            let p = port_running(dir.path(), "exec sleep 600");
            let started = Instant::now();
            assert_stopped_at_deadline(started, p.probe());
            let started = Instant::now();
            assert_stopped_at_deadline(started, p.restart_installed(Some("0.46.0"), None));
        }

        /// Scenario: a reply that fits is still read and parsed through the
        /// bounded path.
        #[test]
        fn a_well_formed_reply_still_parses() {
            let dir = crate::test_temp::tempdir().unwrap();
            let line = serde_json::to_string(&DaemonProbe {
                running: false,
                hello: None,
            })
            .unwrap();
            let p = port_running(dir.path(), &format!("printf '%s\\n' '{line}'"));
            assert!(!p.probe().unwrap().running);
        }

        /// The pid a stand-in recorded for the descendant it started.
        fn recorded_pid(path: &std::path::Path) -> libc::pid_t {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(pid) = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                {
                    return pid;
                }
                assert!(Instant::now() < deadline, "no pid recorded at {path:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// Whether `pid` has stopped running: gone, or a zombie its new parent
        /// has not reaped yet.
        fn exited(pid: libc::pid_t) -> bool {
            // SAFETY: signal 0 only checks that the pid exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            })
        }

        fn assert_cleaned_up(pid: libc::pid_t) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !exited(pid) {
                if Instant::now() >= deadline {
                    // SAFETY: plain signal to the pid this test started.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    panic!("the descendant {pid} was left running");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// Scenario: the stand-in starts a quiet descendant that inherits its
        /// streams — a `ProxyCommand` or jump-route helper — prints a
        /// well-formed reply and exits. The descendant never writes and never
        /// closes the streams, yet the reply is parsed promptly and the
        /// descendant is killed with the session's process group (PRD #1487
        /// re-check R1, parent-exits case).
        #[test]
        fn a_quiet_descendant_holding_the_streams_cannot_outlive_a_finished_command() {
            let dir = crate::test_temp::tempdir().unwrap();
            let pidfile = dir.path().join("descendant.pid");
            let line = serde_json::to_string(&DaemonProbe {
                running: false,
                hello: None,
            })
            .unwrap();
            let p = port_running(
                dir.path(),
                &format!(
                    "sleep 600 &\necho $! > '{}'\nprintf '%s\\n' '{line}'",
                    pidfile.display()
                ),
            );
            let started = Instant::now();
            let probe = p.probe();
            let elapsed = started.elapsed();
            let pid = recorded_pid(&pidfile);
            assert_cleaned_up(pid);
            assert!(
                elapsed < MUST_RETURN_WITHIN,
                "the call waited on the descendant: {elapsed:?}"
            );
            assert!(!probe.unwrap().running);
        }

        /// Scenario: the same quiet descendant, under a command that never
        /// finishes. At the deadline the whole group is killed — the command
        /// and the descendant still holding its streams — and the call returns
        /// instead of waiting for streams nobody will close (parent-killed
        /// case).
        #[test]
        fn a_quiet_descendant_holding_the_streams_is_killed_with_a_stopped_command() {
            let dir = crate::test_temp::tempdir().unwrap();
            let pidfile = dir.path().join("descendant.pid");
            let p = port_running(
                dir.path(),
                &format!(
                    "sleep 600 &\necho $! > '{}'\nexec sleep 600",
                    pidfile.display()
                ),
            );
            let started = Instant::now();
            let result = p.restart_installed(Some("0.46.0"), None);
            let pid = recorded_pid(&pidfile);
            assert_cleaned_up(pid);
            assert_stopped_at_deadline(started, result);
        }

        /// Scenario: a descendant that leaves the session's process group
        /// (`setsid`, as a `ControlPersist` master does) cannot be killed with
        /// it, and still cannot hold the call past its deadline: its streams
        /// are abandoned rather than waited on, whether the command exits or
        /// is stopped.
        #[cfg(target_os = "linux")]
        #[test]
        fn a_descendant_that_left_the_group_still_cannot_hold_the_call() {
            if std::process::Command::new("setsid")
                .arg("true")
                .status()
                .map_or(true, |s| !s.success())
            {
                eprintln!("SKIP: no `setsid` on this host");
                return;
            }
            for tail in ["exit 0", "exec sleep 600"] {
                let dir = crate::test_temp::tempdir().unwrap();
                let pidfile = dir.path().join("descendant.pid");
                let p = port_running(
                    dir.path(),
                    &format!(
                        "setsid sleep 600 &\necho $! > '{}'\n{tail}",
                        pidfile.display()
                    ),
                );
                let started = Instant::now();
                let result = p.probe();
                let elapsed = started.elapsed();
                let pid = recorded_pid(&pidfile);
                // Out of reach of the group kill, so this test cleans it up.
                // SAFETY: plain signal to the pid this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                assert!(
                    elapsed < DEADLINE + Duration::from_secs(5),
                    "`{tail}`: the call waited on an escaped descendant: {elapsed:?}"
                );
                if tail == "exec sleep 600" {
                    assert_stopped_at_deadline(started, result);
                }
            }
        }
    }

    #[test]
    fn the_restart_deadline_leaves_room_for_the_whole_restart_round_trip() {
        assert!(REMOTE_RESTART_DEADLINE > crate::daemon_client::RESTART_REQUEST_TIMEOUT);
        assert!(REMOTE_RESTART_DEADLINE > REMOTE_PROBE_DEADLINE);
        assert!(
            REMOTE_PROBE_DEADLINE.as_secs() > crate::daemon_upgrade::UPGRADE_SSH_CONNECT_TIMEOUT
        );
    }
}
