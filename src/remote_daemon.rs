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
//! line", and issue #1490 added [`SshDaemonPort::start_detached`] beside
//! [`SshDaemonPort::probe`] and [`SshDaemonPort::restart_installed`]. The
//! upgrade flow adapts it to its own port trait in [`crate::daemon_upgrade`],
//! and the start flow uses it directly in [`crate::daemon_start`].

use std::cell::{Cell, RefCell};
use std::time::Duration;

use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::daemon_client::RemoteEndpoint;
use crate::daemon_protocol::RestartStopSet;
use crate::daemon_restart::{
    DaemonProbe, MAX_CONFIRM_HEX_LEN, MAX_CONFIRM_STDIN_LEN, RemoteRestartReport,
    encode_stop_set_hex,
};
use crate::remote::{
    RemoteBinaryPath, RemoteDeckBinary, RemoteEntry, SshError, SshExecutor, SshTarget,
};

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
/// allows ([`crate::daemon_client::RESTART_REQUEST_TIMEOUT`]), which covers
/// everything the daemon may do before it answers — checking the installed
/// build ([`crate::daemon_restart::RESTART_VERIFY_TIMEOUT`]) and waiting for
/// respawns to settle ([`crate::agent_pty::RESPAWN_SETTLE_TIMEOUT`]) — plus a
/// margin. The drain happens after the daemon has answered, so it costs this
/// command nothing.
pub const REMOTE_RESTART_DEADLINE: Duration =
    PLUMBING_START_ALLOWANCE.saturating_add(crate::daemon_client::RESTART_REQUEST_TIMEOUT);

/// How much longer a restart whose confirmation goes on stdin may run than
/// [`REMOTE_RESTART_DEADLINE`]: the remote reads its stdin before it sends the
/// request, for up to [`crate::daemon_restart::CONFIRM_STDIN_TIMEOUT`], and a
/// deadline that did not count that could kill a session whose request had
/// just gone out (issue #1619, Qodo 4236548121).
pub const REMOTE_RESTART_STDIN_EXTRA: Duration = crate::daemon_restart::CONFIRM_STDIN_TIMEOUT;

/// The daemon's worst case before it answers a restart request.
const DAEMON_RESTART_WORST_CASE_MS: u128 = crate::daemon_restart::RESTART_VERIFY_TIMEOUT
    .as_millis()
    + crate::agent_pty::RESPAWN_SETTLE_TIMEOUT.as_millis();

// Both bounds must outlast the daemon's worst case before it answers, or the
// laptop gives up on a restart the daemon is about to accept (PRD #1487 review).
const _: () = assert!(
    crate::daemon_client::RESTART_REQUEST_TIMEOUT.as_millis() > DAEMON_RESTART_WORST_CASE_MS
);
const _: () = assert!(
    REMOTE_RESTART_DEADLINE.as_millis()
        > PLUMBING_START_ALLOWANCE.as_millis() + DAEMON_RESTART_WORST_CASE_MS
);
const _: () = assert!(
    REMOTE_RESTART_DEADLINE.as_millis() + REMOTE_RESTART_STDIN_EXTRA.as_millis()
        > PLUMBING_START_ALLOWANCE.as_millis()
            + crate::daemon_restart::CONFIRM_STDIN_TIMEOUT.as_millis()
            + DAEMON_RESTART_WORST_CASE_MS
);

/// The exit code clap uses for a usage error — what a deck binary that predates
/// a plumbing subcommand exits with when asked to run it.
const CLAP_USAGE_EXIT: i32 = 2;

/// `daemon endpoint`'s exit status when something is at the endpoint and it
/// failed a trust check (`ENDPOINT_UNTRUSTED` in `main.rs`).
const ENDPOINT_UNTRUSTED_EXIT: i32 = 1;
/// `daemon endpoint`'s exit status when nothing usable answered there
/// (`ENDPOINT_UNDETERMINED` in `main.rs`).
const ENDPOINT_UNDETERMINED_EXIT: i32 = 3;

/// What [`SshDaemonPort::start_detached`] runs after the binary.
const START_DETACHED_ARGS: &str = "daemon serve </dev/null >/dev/null 2>&1 &";

/// What `daemon endpoint` said about the remote's endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointAnswer {
    /// A trusted daemon answered a `Hello` there.
    Live,
    /// Nothing is at the endpoint, or a stale socket refused the connection:
    /// no daemon is running there.
    Nothing,
    /// Something is at the endpoint and accepted the connection, but no
    /// usable `Hello` came back: a daemon is there and the check could not
    /// talk to it.
    NoAnswer { stderr: String },
    /// Something is at the endpoint and failed the trust check.
    Refused { stderr: String },
}

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
    /// The subcommand exited 0 but its output was not a reply this build can
    /// parse: none at all, more than the cap, or a line that is not its JSON.
    #[error("the remote command printed no reply this build can parse: {0}")]
    Malformed(String),
    /// Refused here, before anything ran on the remote: the request could not
    /// be passed to the installed build (issue #1619).
    #[error("{0}")]
    NotSent(String),
}

/// A remote machine's daemon, reached through the deck binary installed there.
///
/// `binary` is a [`RemoteDeckBinary`] — the default install or a validated
/// absolute path — so every command can put it unquoted at the start of the
/// remote shell command. It can be repointed after an install moved the deck (a
/// Homebrew install replacing `~/.local/bin`), which is why it sits in a
/// `RefCell`.
pub struct SshDaemonPort<E: SshExecutor> {
    executor: E,
    target: SshTarget,
    binary: RefCell<RemoteDeckBinary>,
    /// The daemon's attach socket on the remote, when the deck-list row names
    /// one (the desktop's "Daemon socket" field). Every command runs with it
    /// as `DOT_AGENT_DECK_ATTACH_SOCKET`, so the probe and the restart reach
    /// the daemon the tunnel reaches rather than the remote shell's default.
    socket: Option<String>,
    /// Whether the last probe through the current binary reported that its
    /// `restart-installed` reads `--confirm-stdin` (issue #1619). `false` until
    /// a probe says otherwise, and again whenever the binary is repointed, so
    /// a build that was never asked is sent only `--confirm-hex`.
    confirm_stdin: Cell<bool>,
    probe_deadline: Duration,
    restart_deadline: Duration,
}

impl<E: SshExecutor> SshDaemonPort<E> {
    pub fn new(executor: E, target: SshTarget, binary: RemoteDeckBinary) -> Self {
        Self {
            executor,
            target,
            binary: RefCell::new(binary),
            socket: None,
            confirm_stdin: Cell::new(false),
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

    /// Reach the daemon at `socket` on the remote instead of that machine's
    /// default attach endpoint.
    pub fn with_socket(mut self, socket: Option<String>) -> Self {
        self.socket = socket.filter(|s| !s.is_empty());
        self
    }

    /// The port for a deck-list row: its ssh target (jump host included), its
    /// recorded binary, and its recorded daemon socket.
    pub fn for_entry(executor: E, entry: &RemoteEntry) -> Self {
        Self::new(executor, entry.ssh_target(), entry.deck_binary())
            .with_socket(entry.socket.clone())
    }

    /// The port for a deck the desktop knows by its [`RemoteEndpoint`]: its
    /// ssh route (user, port, key and jump host), its daemon socket, and the
    /// deck binary `binary` names — the deck-list row's recorded
    /// [`RemoteEntry::binary`] when there is one, otherwise
    /// [`RemoteDeckBinary::DefaultInstall`] (issue #1490).
    pub fn for_endpoint(
        executor: E,
        endpoint: &RemoteEndpoint,
        binary: Option<&RemoteBinaryPath>,
    ) -> Self {
        Self::new(
            executor,
            endpoint.ssh_target(),
            RemoteDeckBinary::recorded_or_default(binary),
        )
        .with_socket(Some(endpoint.socket().to_string()))
    }

    /// The daemon socket every command names, when the deck sets one.
    pub fn socket(&self) -> Option<&str> {
        self.socket.as_deref()
    }

    /// Run later commands through `binary` instead.
    pub fn set_binary(&self, binary: RemoteDeckBinary) {
        *self.binary.borrow_mut() = binary;
        self.confirm_stdin.set(false);
    }

    /// The binary later commands run, as spelled for the remote shell.
    pub fn binary(&self) -> String {
        self.binary.borrow().as_shell_word().to_string()
    }

    /// Whether later commands run the default install, `~/.local/bin`.
    pub fn runs_default_install(&self) -> bool {
        *self.binary.borrow() == RemoteDeckBinary::DefaultInstall
    }

    /// Look for a Homebrew install of the deck on the remote, within the
    /// probe deadline, the way `connect` does when a row's default install is
    /// gone ([`crate::remote::discover_homebrew_binary`], issue #1459).
    /// Changes nothing on the remote, and does not repoint this port.
    pub fn discover_homebrew(&self) -> Result<Option<RemoteBinaryPath>, SshError> {
        crate::remote::discover_homebrew_binary_within(
            &self.executor,
            &self.target,
            self.probe_deadline,
        )
    }

    /// The executor, for a test that asserts what was run.
    #[cfg(test)]
    pub(crate) fn executor_for_tests(&self) -> &E {
        &self.executor
    }

    /// The ssh target every command goes to.
    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    /// `daemon probe --json` on the remote: whether a daemon runs at that
    /// machine's endpoint, and its `Hello` reply. Never starts one.
    pub fn probe(&self) -> Result<DaemonProbe, RemoteDaemonError> {
        self.probe_within(self.probe_deadline)
    }

    /// [`Self::probe`], killed at `budget` when that comes before the port's
    /// own probe deadline — for a caller with less time left than one whole
    /// probe (PRD #1487, Qodo 4202262493). With the production executor the
    /// kill counts whole seconds, rounded down, and a budget under
    /// [`Self::min_probe_budget`] starts nothing and is an error, so the probe
    /// never runs past `budget`.
    pub fn probe_within(&self, budget: Duration) -> Result<DaemonProbe, RemoteDaemonError> {
        let probe: DaemonProbe =
            self.run_json("daemon probe --json", self.probe_deadline.min(budget))?;
        self.confirm_stdin.set(probe.confirm_stdin);
        Ok(probe)
    }

    /// The shortest budget [`Self::probe_within`] honours — the executor's
    /// [`SshExecutor::min_bounded_run`].
    pub fn min_probe_budget(&self) -> Duration {
        self.executor.min_bounded_run()
    }

    /// `daemon restart-installed --json` on the remote: ask that machine's
    /// daemon to restart onto the build installed at its own path.
    /// `expected_version` and `confirm` are passed through to the daemon.
    ///
    /// The confirmed stop set goes as `--confirm-hex`, which every build reads,
    /// whenever its encoding fits [`MAX_CONFIRM_HEX_LEN`]. A larger one goes on
    /// the remote command's stdin when the last [`Self::probe`] reported that
    /// the binary reads it there and the executor can write it (issue #1619).
    /// Otherwise it is refused here as [`RemoteDaemonError::NotSent`] rather
    /// than sent as a command the remote could not start.
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
        let mut input = None;
        if let Some(set) = confirm {
            // Hex: no shell metacharacter can appear in it, and every build
            // reads it, so it is used whenever it fits. Stdin is only for a set
            // that does not: a host whose ssh configuration sets `StdinNull`
            // drops what is written there, and the remote then refuses the
            // empty input with nothing sent (Greptile 4236434468).
            let hex = encode_stop_set_hex(set);
            if hex.len() <= MAX_CONFIRM_HEX_LEN {
                args.push_str(" --confirm-hex ");
                args.push_str(&hex);
            } else if self.confirm_stdin.get() && self.executor.writes_stdin() {
                let json = serde_json::to_vec(set).map_err(|e| {
                    RemoteDaemonError::NotSent(format!("the stop set could not be encoded: {e}"))
                })?;
                if json.len() as u64 > MAX_CONFIRM_STDIN_LEN {
                    return Err(self.too_large(&format!(
                        "{} bytes, over the {MAX_CONFIRM_STDIN_LEN}-byte limit the installed build reads",
                        json.len()
                    )));
                }
                args.push_str(" --confirm-stdin");
                input = Some(json);
            } else {
                return Err(self.too_large(&format!(
                    "{} bytes encoded, over the {MAX_CONFIRM_HEX_LEN}-byte limit of the command \
                     line, and the installed build is too old to read it another way",
                    hex.len()
                )));
            }
        }
        let deadline = if input.is_some() {
            self.restart_deadline
                .saturating_add(REMOTE_RESTART_STDIN_EXTRA)
        } else {
            self.restart_deadline
        };
        let output =
            self.run_bounded_command_with(&self.command(&args), input.as_deref(), deadline)?;
        parse_json_reply(output)
    }

    /// The refusal for a confirmed stop set that cannot be passed to the
    /// installed build: nothing ran on the remote, so the daemon was not asked.
    fn too_large(&self, why: &str) -> RemoteDaemonError {
        RemoteDaemonError::NotSent(format!(
            "the work to stop is too large to pass to the installed build at {} ({why}), so the \
             daemon was not asked to restart; restart it on that machine with \
             `dot-agent-deck daemon restart`",
            self.binary()
        ))
    }

    /// `<binary> --version` on the remote, classified exactly as `connect`'s
    /// binary-version probe classifies it
    /// ([`crate::connect::classify_version_probe`]): the remote's version, or
    /// unreachable / authentication / host key, no deck binary at the path, or
    /// a failure the remote reported. Issue #1490's start runs it first, so a
    /// missing install is named rather than lost to a detached start.
    pub fn version(&self) -> Result<String, crate::connect::VersionProbeFailure> {
        let command = format!("{} --version", self.binary.borrow().as_shell_word());
        crate::connect::classify_version_probe(self.executor.run_capped_within(
            &self.target,
            &command,
            crate::connect::PROBE_VERSION_CAP,
            self.probe_deadline,
        ))
    }

    /// `daemon endpoint` on the remote (issue #1174): the older, plain-text
    /// way to ask whether a daemon answers at that machine's endpoint, kept
    /// for a remote whose deck predates `daemon probe` (PRD #1487). See
    /// [`EndpointAnswer`] for how its exit status reads.
    pub fn endpoint(&self) -> Result<EndpointAnswer, RemoteDaemonError> {
        let output = self.run_bounded("daemon endpoint", self.probe_deadline)?;
        let stderr = crate::remote::scrub_remote_text(output.stderr.trim());
        Ok(match output.status {
            0 => EndpointAnswer::Live,
            CLAP_USAGE_EXIT => return Err(RemoteDaemonError::Unsupported { stderr }),
            ENDPOINT_UNTRUSTED_EXIT => EndpointAnswer::Refused { stderr },
            ENDPOINT_UNDETERMINED_EXIT => {
                let lower = stderr.to_ascii_lowercase();
                if lower.contains("nothing at ") || lower.contains("connection refused") {
                    EndpointAnswer::Nothing
                } else {
                    EndpointAnswer::NoAnswer { stderr }
                }
            }
            status => return Err(RemoteDaemonError::Failed { status, stderr }),
        })
    }

    /// Start `<binary> daemon serve` on the remote, detached, and return
    /// without waiting for it (issue #1490).
    ///
    /// One non-interactive command: `nohup` so the session's hangup does not
    /// reach the daemon, every stream redirected so ssh has no open channel to
    /// wait on, and `&` so the remote shell exits at once. The daemon listens
    /// at the port's socket, passed as `DOT_AGENT_DECK_ATTACH_SOCKET` like
    /// every other command here, which is where `daemon serve` binds its
    /// attach endpoint. Whether it came up is the caller's to check: a
    /// detached start reports nothing about the process it started.
    pub fn start_detached(&self) -> Result<(), RemoteDaemonError> {
        let command = self.command_with("nohup ", START_DETACHED_ARGS);
        let output = self.run_bounded_command(&command, self.probe_deadline)?;
        if output.status != 0 {
            return Err(RemoteDaemonError::Failed {
                status: output.status,
                stderr: crate::remote::scrub_remote_text(output.stderr.trim()),
            });
        }
        Ok(())
    }

    /// `<binary> <args>`, with the configured socket in its environment.
    ///
    /// The binary is a [`RemoteDeckBinary`], which by construction is either a
    /// validated absolute path or the `~/.local/bin` default, both free of
    /// shell metacharacters, and is left unquoted so the remote shell expands
    /// the default's `~`. The socket is whatever the deck list holds, so it is
    /// quoted as one shell word.
    fn command(&self, args: &str) -> String {
        self.command_with("", args)
    }

    /// [`Self::command`] with `wrapper` (`"nohup "`) between the environment
    /// and the binary.
    fn command_with(&self, wrapper: &str, args: &str) -> String {
        let env = self
            .socket
            .as_deref()
            .map(|socket| {
                format!(
                    "env DOT_AGENT_DECK_ATTACH_SOCKET={} ",
                    crate::remote::shell_word(socket)
                )
            })
            .unwrap_or_default();
        format!(
            "{env}{wrapper}{} {args}",
            self.binary.borrow().as_shell_word()
        )
    }

    /// Run `<binary> <args>` bounded by `deadline` and the reply cap, and hand
    /// back what it printed. A stream that reached the cap is not a reply this
    /// build wrote, whatever the exit status.
    fn run_bounded(
        &self,
        args: &str,
        deadline: Duration,
    ) -> Result<crate::remote::SshOutput, RemoteDaemonError> {
        self.run_bounded_command(&self.command(args), deadline)
    }

    fn run_bounded_command(
        &self,
        command: &str,
        deadline: Duration,
    ) -> Result<crate::remote::SshOutput, RemoteDaemonError> {
        self.run_bounded_command_with(command, None, deadline)
    }

    /// [`Self::run_bounded_command`], with `input` written to the command's
    /// stdin when there is one.
    fn run_bounded_command_with(
        &self,
        command: &str,
        input: Option<&[u8]>,
        deadline: Duration,
    ) -> Result<crate::remote::SshOutput, RemoteDaemonError> {
        let capped = match input {
            Some(input) => self.executor.run_capped_within_input(
                &self.target,
                command,
                input,
                REMOTE_DAEMON_REPLY_CAP,
                deadline,
            )?,
            None => self.executor.run_capped_within(
                &self.target,
                command,
                REMOTE_DAEMON_REPLY_CAP,
                deadline,
            )?,
        };
        if capped.truncated {
            return Err(RemoteDaemonError::Malformed(format!(
                "more than {REMOTE_DAEMON_REPLY_CAP} bytes"
            )));
        }
        Ok(capped.output)
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
        parse_json_reply(self.run_bounded(args, deadline)?)
    }
}

/// Parse the last non-empty stdout line of a plumbing command as `T`, after
/// reading its exit status: a usage error is [`RemoteDaemonError::Unsupported`]
/// and any other failure [`RemoteDaemonError::Failed`]. A stream that reached
/// the cap was refused before this, whatever the exit status — and a remote
/// that kept writing past it usually dies of the closed pipe, so its status
/// says nothing useful.
fn parse_json_reply<T: DeserializeOwned>(
    output: crate::remote::SshOutput,
) -> Result<T, RemoteDaemonError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::{AttachResponse, RestartAgent, RestartDaemonReply};
    use crate::remote::SshOutput;

    /// Records each command, and what was written to its stdin, and answers
    /// from a script.
    struct Scripted {
        commands: RefCell<Vec<String>>,
        inputs: RefCell<Vec<Vec<u8>>>,
        reply: SshOutput,
    }

    impl SshExecutor for Scripted {
        fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
            self.commands.borrow_mut().push(command.to_string());
            Ok(self.reply.clone())
        }

        fn run_capped_within_input(
            &self,
            target: &SshTarget,
            command: &str,
            input: &[u8],
            max_capture_bytes: usize,
            _deadline: Duration,
        ) -> Result<crate::remote::CappedOutput, SshError> {
            self.inputs.borrow_mut().push(input.to_vec());
            self.run_capped(target, command, max_capture_bytes)
        }

        fn writes_stdin(&self) -> bool {
            true
        }
    }

    fn port(status: i32, stdout: &str, stderr: &str) -> SshDaemonPort<Scripted> {
        SshDaemonPort::new(
            Scripted {
                commands: RefCell::new(Vec::new()),
                inputs: RefCell::new(Vec::new()),
                reply: SshOutput {
                    status,
                    stdout: stdout.to_string(),
                    stderr: stderr.to_string(),
                },
            },
            SshTarget::parse("u@h", 22, None),
            crate::remote::RemoteDeckBinary::DefaultInstall,
        )
    }

    #[test]
    fn probe_parses_the_last_json_line() {
        let hello = AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION);
        let line = serde_json::to_string(&DaemonProbe {
            running: true,
            hello: Some(hello.clone()),
            confirm_stdin: false,
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

    /// PRD #1487 review: a deck-list row that names the daemon's socket (the
    /// desktop's "Daemon socket" field) reaches that daemon — the probe and
    /// the restart both run with it as `DOT_AGENT_DECK_ATTACH_SOCKET`, quoted
    /// as one shell word — and a row without one runs the bare command.
    #[test]
    fn a_configured_daemon_socket_reaches_every_command() {
        let report = RemoteRestartReport {
            running: false,
            reply: None,
            unsupported: false,
        };
        let line = serde_json::to_string(&report).unwrap();
        let mut entry = RemoteEntry {
            name: "box".into(),
            kind: "ssh".into(),
            host: "u@h".into(),
            port: 22,
            key: None,
            version: "0.46.0".into(),
            added_at: String::new(),
            upgraded_at: None,
            last_connected: None,
            install: None,
            binary: None,
            id: None,
            user: None,
            jump_host: None,
            socket: Some("/run/deck dir/it's.sock".into()),
        };
        let scripted = || Scripted {
            commands: RefCell::new(Vec::new()),
            inputs: RefCell::new(Vec::new()),
            reply: SshOutput {
                status: 0,
                stdout: line.clone(),
                stderr: String::new(),
            },
        };
        let p = SshDaemonPort::for_entry(scripted(), &entry);
        let _ = p.probe();
        p.restart_installed(Some("0.46.0"), None).unwrap();
        let commands = p.executor.commands.borrow().clone();
        let prefix = "env DOT_AGENT_DECK_ATTACH_SOCKET='/run/deck dir/it'\\''s.sock' ~/.local/bin/dot-agent-deck daemon ";
        assert_eq!(commands.len(), 2);
        assert!(
            commands.iter().all(|c| c.starts_with(prefix)),
            "every command must carry the configured socket: {commands:?}"
        );
        assert!(commands[0].ends_with("daemon probe --json"));
        assert!(commands[1].contains("daemon restart-installed --json"));

        entry.socket = None;
        let p = SshDaemonPort::for_entry(scripted(), &entry);
        let _ = p.probe();
        assert_eq!(
            p.executor.commands.borrow().as_slice(),
            ["~/.local/bin/dot-agent-deck daemon probe --json"]
        );
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
        p.set_binary(RemoteDeckBinary::try_from("/opt/homebrew/bin/dot-agent-deck").unwrap());
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
        assert!(p.executor.inputs.borrow().is_empty(), "nothing on stdin");
    }

    fn needs_confirmation_report() -> RemoteRestartReport {
        RemoteRestartReport {
            running: true,
            reply: Some(RestartDaemonReply::NeedsConfirmation {
                at_stake: RestartStopSet::default(),
                stale: false,
            }),
            unsupported: false,
        }
    }

    /// Scenario: the installed build's `daemon probe --json` reports that it
    /// reads `--confirm-stdin`, and the stop set to confirm is larger than the
    /// 128 KiB argument limit once hex-encoded. The restart command carries
    /// `--confirm-stdin` and stays short, and the set's JSON is written to its
    /// stdin (issue #1619).
    #[test]
    fn a_build_that_reads_stdin_is_sent_the_stop_set_there() {
        let probe = serde_json::to_string(&DaemonProbe {
            running: false,
            hello: None,
            confirm_stdin: true,
        })
        .unwrap();
        let p = port(0, &probe, "");
        assert!(p.probe().unwrap().confirm_stdin);

        let report = needs_confirmation_report();
        let p = SshDaemonPort {
            executor: Scripted {
                commands: RefCell::new(Vec::new()),
                inputs: RefCell::new(Vec::new()),
                reply: SshOutput {
                    status: 0,
                    stdout: serde_json::to_string(&report).unwrap(),
                    stderr: String::new(),
                },
            },
            ..p
        };
        let set = crate::daemon_restart::stop_set_over_the_argument_limit();
        assert_eq!(
            p.restart_installed(Some("0.46.0"), Some(&set)).unwrap(),
            report
        );
        let commands = p.executor.commands.borrow();
        assert_eq!(
            commands.as_slice(),
            [
                "~/.local/bin/dot-agent-deck daemon restart-installed --json --expect-version 0.46.0 --confirm-stdin"
            ]
        );
        {
            let inputs = p.executor.inputs.borrow();
            assert_eq!(inputs.len(), 1);
            assert_eq!(
                crate::daemon_restart::read_stop_set(inputs[0].as_slice()).unwrap(),
                set
            );
        }
        drop(commands);

        // A set that fits the command line still goes there, so an ordinary
        // confirmation never depends on stdin (Greptile 4236434468).
        let small = RestartStopSet {
            agents: vec![RestartAgent {
                id: "a1".into(),
                label: "one".into(),
                pane_id: None,
                cwd: None,
            }],
            roles: vec![],
        };
        p.restart_installed(Some("0.46.0"), Some(&small)).unwrap();
        assert!(
            p.executor.commands.borrow()[1]
                .ends_with(&format!("--confirm-hex {}", encode_stop_set_hex(&small))),
            "{:?}",
            p.executor.commands.borrow()
        );
        assert_eq!(p.executor.inputs.borrow().len(), 1, "nothing more on stdin");
    }

    /// Scenario: the installed build never said it reads `--confirm-stdin` —
    /// it was not probed, it predates the flag, or the port was repointed at
    /// another binary since. A small set still goes as `--confirm-hex`; one
    /// whose encoding is past the argument limit is refused here, with nothing
    /// run on the remote and a message naming the restart on that machine
    /// (issue #1619).
    #[test]
    fn a_build_that_does_not_read_stdin_never_gets_the_flag() {
        let probe = serde_json::to_string(&DaemonProbe {
            running: false,
            hello: None,
            confirm_stdin: true,
        })
        .unwrap();
        let p = port(0, &probe, "");
        p.probe().unwrap();
        p.set_binary(RemoteDeckBinary::try_from("/opt/homebrew/bin/dot-agent-deck").unwrap());
        let big = crate::daemon_restart::stop_set_over_the_argument_limit();
        match p.restart_installed(Some("0.46.0"), Some(&big)) {
            Err(RemoteDaemonError::NotSent(why)) => assert!(
                why.contains("/opt/homebrew/bin/dot-agent-deck")
                    && why.contains("dot-agent-deck daemon restart"),
                "{why}"
            ),
            other => panic!("expected the oversized set to be refused here, got {other:?}"),
        }
        assert_eq!(
            p.executor.commands.borrow().len(),
            1,
            "only the probe ran: {:?}",
            p.executor.commands.borrow()
        );

        let small = RestartStopSet {
            agents: vec![RestartAgent {
                id: "a1".into(),
                label: "one".into(),
                pane_id: None,
                cwd: None,
            }],
            roles: vec![],
        };
        let _ = p.restart_installed(Some("0.46.0"), Some(&small));
        let commands = p.executor.commands.borrow();
        assert!(
            commands[1].ends_with(&format!("--confirm-hex {}", encode_stop_set_hex(&small))),
            "{commands:?}"
        );
        assert!(p.executor.inputs.borrow().is_empty(), "nothing on stdin");
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
                crate::remote::RemoteDeckBinary::DefaultInstall,
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

        /// Scenario: through the production executor, a remote whose probe
        /// reports `--confirm-stdin` is asked to restart with a stop set past
        /// the 128 KiB argument limit once hex-encoded. The stand-in `ssh`
        /// saves its stdin and its last argument, as a remote shell would get
        /// them: the whole set arrives on stdin, more than a pipe buffer of it,
        /// and the command is short (issue #1619).
        #[test]
        fn a_stop_set_past_the_argument_limit_reaches_the_remote_on_stdin() {
            let dir = crate::test_temp::tempdir().unwrap();
            let d = dir.path();
            let probe = serde_json::to_string(&DaemonProbe {
                running: true,
                hello: None,
                confirm_stdin: true,
            })
            .unwrap();
            std::fs::write(d.join("probe.json"), probe).unwrap();
            let report = needs_confirmation_report();
            std::fs::write(
                d.join("report.json"),
                serde_json::to_string(&report).unwrap(),
            )
            .unwrap();
            let body = format!(
                "dir='{}'\nfor last; do :; done\n\
                 case \"$last\" in\n\
                 *'daemon probe'*) cat \"$dir/probe.json\" ;;\n\
                 *restart-installed*) cat > \"$dir/stdin\"; \
                 printf '%s' \"$last\" > \"$dir/command\"; \
                 printf '%s\\n' \"$@\" > \"$dir/args\"; cat \"$dir/report.json\" ;;\n\
                 esac",
                d.display()
            );
            let p = port_running(d, &body)
                .with_deadlines(Duration::from_secs(20), Duration::from_secs(20));
            let probed = p.probe().unwrap();
            assert!(probed.confirm_stdin, "{probed:?}");
            let set = crate::daemon_restart::stop_set_over_the_argument_limit();
            let started = Instant::now();
            assert_eq!(
                p.restart_installed(Some("0.46.0"), Some(&set)).unwrap(),
                report
            );
            assert!(started.elapsed() < MUST_RETURN_WITHIN);
            let command = std::fs::read_to_string(d.join("command")).unwrap();
            assert_eq!(
                command,
                "~/.local/bin/dot-agent-deck daemon restart-installed --json --expect-version 0.46.0 --confirm-stdin"
            );
            let args = std::fs::read_to_string(d.join("args")).unwrap();
            assert!(
                args.lines().any(|arg| arg == "-T"),
                "a session carrying input asks for no terminal: {args}"
            );
            let stdin = std::fs::read(d.join("stdin")).unwrap();
            assert!(stdin.len() > 64 * 1024, "{} bytes", stdin.len());
            assert_eq!(
                crate::daemon_restart::read_stop_set(stdin.as_slice()).unwrap(),
                set
            );
        }

        /// Scenario: a remote restart command that answers and exits without
        /// reading the stop set written to its stdin, leaving behind a
        /// descendant that holds that stdin and reads it only three seconds
        /// later. The call returns at once rather than when the payload is
        /// consumed, and the writer has stopped by the time the descendant
        /// reads: it gets what fit in the pipe, then end of input, not the
        /// whole set. A writer still blocked would deliver all of it
        /// (issue #1619).
        #[test]
        fn a_remote_that_never_reads_its_stdin_stops_the_writer() {
            let dir = crate::test_temp::tempdir().unwrap();
            let d = dir.path();
            let report = needs_confirmation_report();
            let line = serde_json::to_string(&report).unwrap();
            std::fs::write(d.join("report.json"), line).unwrap();
            let count = d.join("count");
            let body = format!(
                // A background job's stdin is `/dev/null` unless it is
                // handed one explicitly, so the pipe goes through fd 3.
                "exec 3<&0\n\
                 (sleep 3; wc -c <&3 > '{count}.tmp'; mv '{count}.tmp' '{count}') >/dev/null 2>&1 &\n\
                 exec cat '{report}' </dev/null 3<&-",
                count = count.display(),
                report = d.join("report.json").display()
            );
            let p = port_running(d, &body);
            p.confirm_stdin.set(true);
            let set = crate::daemon_restart::stop_set_over_the_argument_limit();
            let payload = serde_json::to_vec(&set).unwrap().len();
            let started = Instant::now();
            assert_eq!(
                p.restart_installed(Some("0.46.0"), Some(&set)).unwrap(),
                report
            );
            assert!(
                started.elapsed() < DEADLINE,
                "the unread payload held the call: {:?}",
                started.elapsed()
            );
            let until = Instant::now() + Duration::from_secs(15);
            while !count.exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            let read: usize = std::fs::read_to_string(&count)
                .expect("the descendant reached end of input")
                .trim()
                .parse()
                .unwrap();
            assert!(
                read < payload,
                "the writer kept writing after the call returned: {read} of {payload} bytes"
            );
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

        /// Scenario: a caller with one second left probes a remote that never
        /// answers, through a port whose own probe deadline is a minute. The
        /// probe is killed at the caller's second, not the port's minute
        /// (PRD #1487, Qodo 4202262493).
        #[test]
        fn a_probe_within_a_shorter_budget_stops_at_that_budget() {
            let dir = crate::test_temp::tempdir().unwrap();
            let p = port_running(dir.path(), "exec sleep 600")
                .with_deadlines(Duration::from_secs(60), Duration::from_secs(60));
            let started = Instant::now();
            let result = p.probe_within(Duration::from_secs(1));
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(10),
                "the probe ran past its budget: {elapsed:?}"
            );
            match result {
                Err(RemoteDaemonError::Ssh(SshError::Other { detail, .. })) => assert!(
                    detail.contains("did not finish within 1s"),
                    "unexpected detail: {detail}"
                ),
                other => panic!("expected the budget to stop the probe, got {other:?}"),
            }
        }

        /// Scenario: a caller with less than one second left probes. The
        /// executor's kill counts whole seconds, so rather than granting the
        /// session a whole second it refuses at once and starts nothing; a
        /// fractional budget above a second is rounded down, never up (PRD
        /// #1487, review item 13).
        #[test]
        fn a_probe_never_runs_past_a_fractional_budget() {
            let dir = crate::test_temp::tempdir().unwrap();
            let marker = dir.path().join("started");
            let p = port_running(
                dir.path(),
                &format!("touch '{}'\nexec sleep 600", marker.display()),
            )
            .with_deadlines(Duration::from_secs(60), Duration::from_secs(60));

            let budget = Duration::from_millis(200);
            let started = Instant::now();
            let result = p.probe_within(budget);
            let elapsed = started.elapsed();
            assert!(elapsed < budget, "the refusal took {elapsed:?}");
            match result {
                Err(RemoteDaemonError::Ssh(SshError::Other { detail, .. })) => assert!(
                    detail.contains("was not started"),
                    "unexpected detail: {detail}"
                ),
                other => panic!("expected a sub-second budget to be refused, got {other:?}"),
            }
            assert!(!marker.exists(), "a session was started");

            let budget = Duration::from_millis(1_900);
            let started = Instant::now();
            let result = p.probe_within(budget);
            let elapsed = started.elapsed();
            assert!(
                elapsed < budget,
                "the probe ran past its budget: {elapsed:?}"
            );
            match result {
                Err(RemoteDaemonError::Ssh(SshError::Other { detail, .. })) => assert!(
                    detail.contains("did not finish within 1s"),
                    "unexpected detail: {detail}"
                ),
                other => panic!("expected the budget to stop the probe, got {other:?}"),
            }
        }

        /// Scenario: a reply that fits is still read and parsed through the
        /// bounded path.
        #[test]
        fn a_well_formed_reply_still_parses() {
            let dir = crate::test_temp::tempdir().unwrap();
            let line = serde_json::to_string(&DaemonProbe {
                running: false,
                hello: None,
                confirm_stdin: false,
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
                confirm_stdin: false,
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
