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

use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::daemon_protocol::RestartStopSet;
use crate::daemon_restart::{DaemonProbe, RemoteRestartReport, encode_stop_set_hex};
use crate::remote::{RemoteEntry, SshError, SshExecutor, SshTarget};

/// Upper bound on what one plumbing command may print. A `Hello` reply with a
/// full capability list and a running-agents summary is a few KiB; anything
/// near this is not a reply this build wrote.
pub const REMOTE_DAEMON_REPLY_CAP: usize = 256 * 1024;

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
}

impl<E: SshExecutor> SshDaemonPort<E> {
    pub fn new(executor: E, target: SshTarget, binary: impl Into<String>) -> Self {
        Self {
            executor,
            target,
            binary: RefCell::new(binary.into()),
        }
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
        self.run_json("daemon probe --json")
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
        self.run_json(&args)
    }

    /// Run `<binary> <args>` and parse the last non-empty stdout line as `T`.
    fn run_json<T: DeserializeOwned>(&self, args: &str) -> Result<T, RemoteDaemonError> {
        // The binary is a `RemoteBinaryPath` or the `~/.local/bin` constant,
        // both free of shell metacharacters, and left unquoted so the remote
        // shell expands `~` (see `RemoteEntry::remote_binary`).
        let command = format!("{} {args}", self.binary.borrow());
        let capped = self
            .executor
            .run_capped(&self.target, &command, REMOTE_DAEMON_REPLY_CAP)?;
        let output = capped.output;
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
        if capped.truncated {
            return Err(RemoteDaemonError::Malformed(format!(
                "more than {REMOTE_DAEMON_REPLY_CAP} bytes"
            )));
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
}
