//! Start a deck's daemon from a client, and say why a deck is not connected
//! (issue #1490).
//!
//! Two decisions live here and nowhere else, so no client carries its own copy
//! of either (PRD #1487's D1, applied to starting):
//!
//! - **Why a deck is not connected** — [`DisconnectedReason`]: no daemon is
//!   running there, a daemon is running and the app is not connected to it, or
//!   the app cannot tell (the host is unreachable, ssh refused the login, the
//!   deck is not installed there). [`DisconnectedReason::action`] turns that
//!   into the one control a client offers: **Start daemon** only when nothing
//!   is running, **Reconnect** otherwise.
//! - **The start procedure** — [`start_local`] and [`start_remote`]: check,
//!   start detached, wait for the daemon to answer at the deck's socket, and
//!   classify any failure into a [`StartOutcome`] in the user's terms.
//!
//! A local deck is probed by connecting to its socket and started through the
//! same lazy-spawn the TUI uses ([`crate::daemon_attach::ensure_daemon_running`]).
//! A remote deck is asked over ssh, through the deck binary installed there
//! ([`crate::remote_daemon::SshDaemonPort`], the port PRD #1487 built for the
//! upgrade): `--version` first, classified exactly as `connect` classifies it
//! ([`crate::connect::classify_version_probe`]), then `daemon probe --json` —
//! or `daemon endpoint` on a deck that predates it — and the detached
//! `daemon serve`. Every remote command carries the deck's configured socket as
//! `DOT_AGENT_DECK_ATTACH_SOCKET`, which is where `daemon serve` binds its
//! attach endpoint, so the daemon it starts is the one the tunnel looks for.
//!
//! **Nothing here is a wire verb.** There is no daemon to send "start" to, so
//! this module puts nothing on the TUI↔daemon protocol and owes no
//! `PROTOCOL_VERSION` bump (CLAUDE.md rules 12 and 18).
//!
//! The remote half is sync, like [`crate::daemon_upgrade`]: its ssh calls block,
//! so a desktop caller runs it inside `spawn_blocking`.

use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::connect::VersionProbeFailure;
use crate::daemon_attach::{AttachError, ensure_daemon_running};
use crate::daemon_client::{Endpoint, LocalEndpoint, RemoteEndpoint};
use crate::remote::{RemoteBinaryPath, SshError, SshExecutor, SystemSshExecutor};
use crate::remote_daemon::{EndpointAnswer, RemoteDaemonError, SshDaemonPort};

/// How a local deck's host is named to the user.
pub const THIS_MACHINE: &str = "this machine";

/// How long the remote start waits for the daemon it started to answer at the
/// deck's socket: the local lazy-spawn's own budget
/// ([`crate::daemon_attach::DAEMON_START_POLL_TIMEOUT`], which covers the
/// daemon's pre-bind login-shell capture) plus room for the ssh round trips
/// that each check costs.
pub const REMOTE_START_WAIT: Duration =
    crate::daemon_attach::DAEMON_START_POLL_TIMEOUT.saturating_add(Duration::from_secs(10));

/// How often the remote start checks whether the daemon answers yet. Each
/// check is one ssh session, so this is a pause between sessions rather than a
/// tight poll.
pub const REMOTE_START_POLL: Duration = Duration::from_secs(1);

/// ssh's connect phase for every command here: the tunnel's own budget,
/// shorter than the upgrade's because a person is watching the deck and an
/// unreachable host should say so promptly.
pub const START_SSH_CONNECT_TIMEOUT: u64 = 10;

/// The laptop-side bound on each remote command: the connect phase, then the
/// remote binary's own bounded handshake with its daemon.
pub const REMOTE_COMMAND_DEADLINE: Duration = Duration::from_secs(START_SSH_CONNECT_TIMEOUT + 15);

/// The ssh executor every remote check and start uses: PRD #1487's
/// [`SystemSshExecutor`] in batch mode with the upgrade's keepalives
/// ([`crate::daemon_upgrade::UPGRADE_SSH_ALIVE_INTERVAL`] /
/// [`crate::daemon_upgrade::UPGRADE_SSH_ALIVE_COUNT_MAX`]), connecting within
/// [`START_SSH_CONNECT_TIMEOUT`].
///
/// Every session is an observation session ([`SystemSshExecutor::observing`])
/// that requires an already-trusted host key
/// ([`SystemSshExecutor::requiring_known_host_key`]). The desktop runs the
/// checks unattended, repeatedly while a remote deck is disconnected, so a user
/// `Host` block's `ForwardAgent`, `ForwardX11`, `GSSAPIDelegateCredentials`,
/// `PermitLocalCommand`, forwards or shared master would otherwise reach a host
/// the app is not even connected to, with nobody pressing anything. Starting
/// needs a login to the host, never a credential delegated to it. The host key
/// is held to the tunnel's policy, so a user `StrictHostKeyChecking accept-new`
/// or `no` cannot have a check or start accept a key the tunnel then refuses.
pub fn start_ssh_executor() -> SystemSshExecutor {
    SystemSshExecutor::with_keepalive(
        START_SSH_CONNECT_TIMEOUT,
        crate::daemon_upgrade::UPGRADE_SSH_ALIVE_INTERVAL,
        crate::daemon_upgrade::UPGRADE_SSH_ALIVE_COUNT_MAX,
    )
    .observing()
    .requiring_known_host_key()
}

/// Bound on one local socket check.
const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The name of `endpoint`'s host as the user reads it — in the confirm dialog
/// ("start the daemon on …") and in every message here: [`THIS_MACHINE`] for a
/// local deck, `user@host[:port]` ([`RemoteEndpoint::describe`]) for a remote
/// one.
pub fn host_label(endpoint: &Endpoint) -> String {
    match endpoint {
        Endpoint::Local(_) => THIS_MACHINE.to_string(),
        Endpoint::Remote(remote) => remote.describe(),
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Why a deck the app is not connected to is not connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DisconnectedReason {
    /// Nothing is listening at the deck's socket: no daemon is running there.
    NotRunning,
    /// A daemon is listening at the deck's socket, and the app is not
    /// connected to it — the connection or the handshake failed, or it was
    /// refused.
    RunningNotConnected,
    /// The app cannot tell whether a daemon is running, and `problem` says
    /// why. Starting one could not succeed either, so the client offers a
    /// retry.
    Unknown(StartProblem),
}

/// The one control a client offers on a deck it is not connected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DisconnectedAction {
    /// Start a daemon there.
    StartDaemon,
    /// Try the connection again.
    Reconnect,
}

impl DisconnectedReason {
    /// What a client shows for a deck whose check has not answered yet: it
    /// cannot tell, so it offers **Reconnect** until the answer arrives.
    pub fn not_checked_yet(host: &str) -> Self {
        Self::Unknown(StartProblem::new(
            StartFailure::NotCheckedYet,
            format!("Checking whether a daemon is running on {host}."),
            None,
        ))
    }

    /// Which control to offer: **Start daemon** only when no daemon is
    /// running, **Reconnect** when one is or when the app cannot tell. Never
    /// both.
    pub fn action(&self) -> DisconnectedAction {
        match self {
            Self::NotRunning => DisconnectedAction::StartDaemon,
            Self::RunningNotConnected | Self::Unknown(_) => DisconnectedAction::Reconnect,
        }
    }

    /// One sentence for the user, naming `host`.
    pub fn summary(&self, host: &str) -> String {
        match self {
            Self::NotRunning => format!("No daemon is running on {host}."),
            Self::RunningNotConnected => format!(
                "A daemon is running on {host}, but the app is not connected to it. Reconnect to try again."
            ),
            Self::Unknown(problem) => problem.message.clone(),
        }
    }

    /// The technical detail behind [`Self::summary`], when there is any.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Unknown(problem) => problem.detail.as_deref(),
            Self::NotRunning | Self::RunningNotConnected => None,
        }
    }
}

/// What kind of problem stopped a start, or a check of whether a daemon runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartFailure {
    /// ssh could not reach the host.
    HostUnreachable,
    /// ssh reached the host and could not log in.
    AuthFailed,
    /// The host's ssh key is not trusted yet.
    HostKeyNotTrusted,
    /// `dot-agent-deck` is not installed on the host.
    NotInstalled,
    /// The `dot-agent-deck` on the host is too old to report on its daemon.
    TooOld,
    /// The daemon was started and did not answer at the deck's socket.
    DidNotAnswer,
    /// Starting the daemon failed.
    StartFailed,
    /// Checking whether a daemon is running failed for another reason.
    CheckFailed,
    /// The check has not answered yet (a remote check takes an ssh round
    /// trip, so a client may show a deck before its first answer).
    NotCheckedYet,
}

/// A problem in the user's terms: what kind, the sentence to show, and the
/// technical detail behind it for a disclosure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartProblem {
    pub failure: StartFailure,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl StartProblem {
    fn new(failure: StartFailure, message: String, detail: Option<String>) -> Self {
        Self {
            failure,
            message,
            detail: detail.filter(|d| !d.trim().is_empty()),
        }
    }
}

/// What a start did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum StartOutcome {
    /// A daemon was started and answers at the deck's socket.
    Started,
    /// A daemon was already running there; nothing was started.
    AlreadyRunning,
    /// Nothing answers at the deck's socket, and `problem` says why.
    Failed(StartProblem),
}

impl StartOutcome {
    /// Whether a daemon now answers at the deck's socket.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Started | Self::AlreadyRunning)
    }

    /// One sentence for the user, naming `host`.
    pub fn summary(&self, host: &str) -> String {
        match self {
            Self::Started => format!("Started the daemon on {host}."),
            Self::AlreadyRunning => {
                format!("A daemon was already running on {host}, so nothing was started.")
            }
            Self::Failed(problem) => problem.message.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Local
// ---------------------------------------------------------------------------

/// Why the local deck at `endpoint` is not connected: nothing listening at its
/// socket (it is missing, or a stale socket refuses the connection) is
/// [`DisconnectedReason::NotRunning`]; a listener that accepts the connection
/// is a running daemon the app is not connected to, whatever then failed.
pub async fn probe_local(endpoint: &LocalEndpoint) -> DisconnectedReason {
    let connect = crate::platform::ipc::IpcStream::connect(endpoint.path());
    match tokio::time::timeout(LOCAL_PROBE_TIMEOUT, connect).await {
        Ok(Ok(_stream)) => DisconnectedReason::RunningNotConnected,
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            DisconnectedReason::NotRunning
        }
        Ok(Err(error)) => DisconnectedReason::Unknown(StartProblem::new(
            StartFailure::CheckFailed,
            format!("The app could not tell whether a daemon is running on {THIS_MACHINE}."),
            Some(format!("{}: {error}", endpoint.path().display())),
        )),
        // Something holds the endpoint and does not complete a connection: a
        // listener is there, it is just not answering.
        Err(_elapsed) => DisconnectedReason::RunningNotConnected,
    }
}

/// Start the daemon for the local deck at `endpoint`: `spawn` starts it
/// detached (the client's own build), under the same lock and wait the TUI's
/// lazy-spawn uses, polling every `poll_interval` for up to `poll_timeout`.
pub async fn start_local<F>(
    endpoint: &LocalEndpoint,
    state_dir: &Path,
    spawn: F,
    poll_interval: Duration,
    poll_timeout: Duration,
) -> StartOutcome
where
    F: FnOnce() -> std::io::Result<()>,
{
    if probe_local(endpoint).await == DisconnectedReason::RunningNotConnected {
        return StartOutcome::AlreadyRunning;
    }
    match ensure_daemon_running(endpoint, state_dir, spawn, poll_interval, poll_timeout).await {
        Ok(()) => StartOutcome::Started,
        Err(error @ AttachError::DaemonStartTimeout { .. }) => {
            StartOutcome::Failed(StartProblem::new(
                StartFailure::DidNotAnswer,
                format!(
                    "The daemon was started on {THIS_MACHINE} but did not answer at {} in time.",
                    endpoint.path().display()
                ),
                Some(error.to_string()),
            ))
        }
        Err(error) => StartOutcome::Failed(StartProblem::new(
            StartFailure::StartFailed,
            format!("Could not start the daemon on {THIS_MACHINE}."),
            Some(error.to_string()),
        )),
    }
}

// ---------------------------------------------------------------------------
// Remote
// ---------------------------------------------------------------------------

/// A remote deck as this module reaches it: the ssh port to its machine and
/// how its host is named to the user.
pub struct RemoteDeck<E: SshExecutor> {
    pub port: SshDaemonPort<E>,
    pub host: String,
}

impl RemoteDeck<SystemSshExecutor> {
    /// The production deck for `endpoint`, reached with [`start_ssh_executor`]
    /// and every command bounded by [`REMOTE_COMMAND_DEADLINE`]. `binary` is
    /// the deck-list row's recorded [`crate::remote::RemoteEntry::binary`] when
    /// it has one; `None` runs the default install.
    pub fn for_endpoint(endpoint: &RemoteEndpoint, binary: Option<&RemoteBinaryPath>) -> Self {
        let mut deck = Self::with_executor(start_ssh_executor(), endpoint, binary);
        deck.port = deck.port.with_deadlines(
            REMOTE_COMMAND_DEADLINE,
            crate::remote_daemon::REMOTE_RESTART_DEADLINE,
        );
        deck
    }
}

impl<E: SshExecutor> RemoteDeck<E> {
    /// [`RemoteDeck::for_endpoint`] over `executor`.
    pub fn with_executor(
        executor: E,
        endpoint: &RemoteEndpoint,
        binary: Option<&RemoteBinaryPath>,
    ) -> Self {
        Self {
            port: SshDaemonPort::for_endpoint(executor, endpoint, binary),
            host: endpoint.describe(),
        }
    }

    fn socket(&self) -> String {
        self.port
            .socket()
            .map_or_else(|| "its default socket".to_string(), str::to_string)
    }
}

/// Timing of [`start_remote`]'s wait for the daemon it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartTiming {
    /// The longest the start waits for the daemon to answer.
    pub wait: Duration,
    /// The pause between two checks.
    pub poll: Duration,
}

impl Default for StartTiming {
    fn default() -> Self {
        Self {
            wait: REMOTE_START_WAIT,
            poll: REMOTE_START_POLL,
        }
    }
}

/// Why the remote deck is not connected, asked on its host over ssh: is a
/// daemon listening at the deck's socket? Never starts one.
///
/// `daemon probe --json` answers on a deck from PRD #1487 on; an older deck's
/// `daemon endpoint` (issue #1174) answers the same question in exit codes.
pub fn probe_remote<E: SshExecutor>(deck: &RemoteDeck<E>) -> DisconnectedReason {
    match deck.port.probe() {
        Ok(probe) if probe.running => DisconnectedReason::RunningNotConnected,
        Ok(_) => DisconnectedReason::NotRunning,
        Err(RemoteDaemonError::Unsupported { .. }) => match deck.port.endpoint() {
            Ok(EndpointAnswer::Live | EndpointAnswer::NoAnswer { .. }) => {
                DisconnectedReason::RunningNotConnected
            }
            Ok(EndpointAnswer::Nothing) => DisconnectedReason::NotRunning,
            Ok(EndpointAnswer::Refused { stderr }) => {
                DisconnectedReason::Unknown(StartProblem::new(
                    StartFailure::CheckFailed,
                    format!(
                        "Something at {} on {} is not a daemon the app can trust, so it cannot tell whether the deck's daemon is running.",
                        deck.socket(),
                        deck.host
                    ),
                    Some(stderr),
                ))
            }
            Err(error) => DisconnectedReason::Unknown(remote_problem(deck, error)),
        },
        Err(error) => DisconnectedReason::Unknown(remote_problem(deck, error)),
    }
}

/// Start the remote deck's daemon: check the deck binary is there (`connect`'s
/// `--version` probe), check whether a daemon already runs at the deck's
/// socket, start `daemon serve` detached at that socket, and wait up to
/// `timing.wait` for it to answer there.
pub fn start_remote<E: SshExecutor>(deck: &RemoteDeck<E>, timing: StartTiming) -> StartOutcome {
    if let Err(failure) = deck.port.version() {
        return StartOutcome::Failed(version_problem(deck, failure));
    }
    match probe_remote(deck) {
        DisconnectedReason::RunningNotConnected => return StartOutcome::AlreadyRunning,
        DisconnectedReason::NotRunning => {}
        DisconnectedReason::Unknown(problem) => return StartOutcome::Failed(problem),
    }
    if let Err(error) = deck.port.start_detached() {
        let problem = match error {
            RemoteDaemonError::Ssh(ssh) => ssh_problem(&deck.host, &ssh),
            other => StartProblem::new(
                StartFailure::StartFailed,
                format!("Could not start the daemon on {}.", deck.host),
                Some(other.to_string()),
            ),
        };
        return StartOutcome::Failed(problem);
    }

    // The last check's problem, if it could not tell, is the detail of the
    // failure below.
    let deadline = Instant::now() + timing.wait;
    let last_problem = loop {
        std::thread::sleep(timing.poll);
        let problem = match probe_remote(deck) {
            DisconnectedReason::RunningNotConnected => return StartOutcome::Started,
            DisconnectedReason::NotRunning => None,
            DisconnectedReason::Unknown(problem) => Some(problem),
        };
        if Instant::now() >= deadline {
            break problem;
        }
    };
    StartOutcome::Failed(StartProblem::new(
        StartFailure::DidNotAnswer,
        format!(
            "The daemon was started on {} but did not answer at {} within {}s. Check that this deck's daemon socket setting is where the daemon listens.",
            deck.host,
            deck.socket(),
            timing.wait.as_secs()
        ),
        last_problem.map(|problem| match problem.detail {
            Some(detail) => format!("{} {detail}", problem.message),
            None => problem.message,
        }),
    ))
}

/// An ssh failure, in the user's terms.
fn ssh_problem(host: &str, error: &SshError) -> StartProblem {
    let detail = Some(crate::connect::ssh_error_detail(error));
    match error {
        SshError::AuthFailed { .. } => StartProblem::new(
            StartFailure::AuthFailed,
            format!(
                "ssh could not log in to {host}. Check the key in this deck's settings, or your ~/.ssh/config."
            ),
            detail,
        ),
        SshError::HostKeyVerificationFailed { remedy, .. } => StartProblem::new(
            StartFailure::HostKeyNotTrusted,
            format!(
                "The ssh host key of {host} is not trusted yet. Run `{remedy}` in a terminal once to check and accept it."
            ),
            None,
        ),
        SshError::ConnectionRefused { .. } | SshError::Io { .. } | SshError::Other { .. } => {
            StartProblem::new(
                StartFailure::HostUnreachable,
                format!(
                    "The app cannot reach {host} over ssh. Check that the host is up and reachable from this machine."
                ),
                detail,
            )
        }
    }
}

fn not_installed(deck: &RemoteDeck<impl SshExecutor>) -> StartProblem {
    StartProblem::new(
        StartFailure::NotInstalled,
        format!(
            "dot-agent-deck is not installed on {}. Install it there with `dot-agent-deck remote add`.",
            deck.host
        ),
        Some(format!("no deck binary at {}", deck.port.binary())),
    )
}

/// What the `--version` probe's failure means for a start.
fn version_problem(
    deck: &RemoteDeck<impl SshExecutor>,
    failure: VersionProbeFailure,
) -> StartProblem {
    match failure {
        VersionProbeFailure::Ssh(error) => ssh_problem(&deck.host, &error),
        VersionProbeFailure::BinaryMissing => not_installed(deck),
        VersionProbeFailure::Truncated => StartProblem::new(
            StartFailure::CheckFailed,
            format!(
                "The dot-agent-deck on {} answered with far more output than a version check prints, so the app did not trust it.",
                deck.host
            ),
            None,
        ),
        VersionProbeFailure::Failed { detail } => StartProblem::new(
            StartFailure::CheckFailed,
            format!("The dot-agent-deck on {} failed to run.", deck.host),
            Some(detail),
        ),
    }
}

/// What a failed remote daemon command means, in the user's terms.
fn remote_problem(deck: &RemoteDeck<impl SshExecutor>, error: RemoteDaemonError) -> StartProblem {
    match error {
        RemoteDaemonError::Ssh(error) => ssh_problem(&deck.host, &error),
        RemoteDaemonError::Unsupported { stderr } => StartProblem::new(
            StartFailure::TooOld,
            format!(
                "The dot-agent-deck on {} is too old to report whether its daemon is running. Upgrade it with `dot-agent-deck remote upgrade`.",
                deck.host
            ),
            Some(stderr),
        ),
        RemoteDaemonError::Failed { status, stderr }
            if crate::connect::is_missing_binary(status, &stderr) =>
        {
            not_installed(deck)
        }
        other => StartProblem::new(
            StartFailure::CheckFailed,
            format!(
                "The app could not tell whether a daemon is running on {}.",
                deck.host
            ),
            Some(other.to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_restart::DaemonProbe;
    use crate::remote::{SshOutput, SshTarget};
    use crate::remote_tunnel::{HostAlias, Hostname, KeyPath, RemoteSocketPath, SshUser};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// One scripted answer: `Ok` is what the remote printed, `Err` an ssh
    /// failure.
    type Answer = Result<SshOutput, SshError>;

    /// Records every ssh call (target and command) and answers from a script,
    /// keyed by which plumbing command was run. A command whose queue is empty
    /// repeats its last answer, so a wait loop can poll it any number of times.
    #[derive(Default)]
    struct FakeSsh {
        calls: RefCell<Vec<(SshTarget, String)>>,
        version: RefCell<VecDeque<Answer>>,
        probe: RefCell<VecDeque<Answer>>,
        endpoint: RefCell<VecDeque<Answer>>,
        start: RefCell<VecDeque<Answer>>,
    }

    fn take(queue: &RefCell<VecDeque<Answer>>, command: &str) -> Answer {
        let mut queue = queue.borrow_mut();
        match queue.len() {
            0 => panic!("no scripted answer for `{command}`"),
            1 => clone_answer(queue.front().expect("one answer")),
            _ => queue.pop_front().expect("an answer"),
        }
    }

    fn clone_answer(answer: &Answer) -> Answer {
        match answer {
            Ok(output) => Ok(output.clone()),
            Err(SshError::ConnectionRefused { host, port, detail }) => {
                Err(SshError::ConnectionRefused {
                    host: host.clone(),
                    port: *port,
                    detail: detail.clone(),
                })
            }
            Err(SshError::AuthFailed { target, detail }) => Err(SshError::AuthFailed {
                target: target.clone(),
                detail: detail.clone(),
            }),
            Err(SshError::HostKeyVerificationFailed { target, remedy }) => {
                Err(SshError::HostKeyVerificationFailed {
                    target: target.clone(),
                    remedy: remedy.clone(),
                })
            }
            Err(SshError::Other { target, detail }) => Err(SshError::Other {
                target: target.clone(),
                detail: detail.clone(),
            }),
            Err(SshError::Io { .. }) => panic!("the script does not use Io errors"),
        }
    }

    impl SshExecutor for FakeSsh {
        fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
            self.calls
                .borrow_mut()
                .push((target.clone(), command.to_string()));
            if command.ends_with(" --version") {
                take(&self.version, command)
            } else if command.ends_with("daemon probe --json") {
                take(&self.probe, command)
            } else if command.ends_with("daemon endpoint") {
                take(&self.endpoint, command)
            } else if command.contains("daemon serve") {
                take(&self.start, command)
            } else {
                panic!("unexpected remote command `{command}`")
            }
        }
    }

    fn out(status: i32, stdout: &str, stderr: &str) -> Answer {
        Ok(SshOutput {
            status,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        })
    }

    fn version_ok() -> Answer {
        out(0, "dot-agent-deck 0.45.1\n", "")
    }

    fn probe_says(running: bool) -> Answer {
        let hello = running.then(|| {
            crate::daemon_protocol::AttachResponse::hello(crate::daemon_protocol::PROTOCOL_VERSION)
        });
        let line = serde_json::to_string(&DaemonProbe { running, hello }).unwrap();
        out(0, &line, "")
    }

    fn unreachable() -> Answer {
        Err(SshError::ConnectionRefused {
            host: "build-box".into(),
            port: 2222,
            detail: "ssh: connect to host build-box port 2222: Connection timed out".into(),
        })
    }

    /// A remote deck with every ssh detail set: user, port, key, jump host and
    /// its own daemon socket.
    fn endpoint() -> RemoteEndpoint {
        RemoteEndpoint::new(
            Hostname::parse("build-box").unwrap(),
            RemoteSocketPath::parse("/run/deck/attach.sock").unwrap(),
        )
        .with_user(SshUser::parse("deploy").unwrap())
        .with_port(2222)
        .with_key(KeyPath::parse("/home/me/.ssh/id_deck").unwrap())
        .with_jump(HostAlias::parse("bastion").unwrap())
    }

    fn deck(fake: FakeSsh) -> RemoteDeck<FakeSsh> {
        RemoteDeck::with_executor(fake, &endpoint(), None)
    }

    fn quick() -> StartTiming {
        StartTiming {
            wait: Duration::from_millis(30),
            poll: Duration::from_millis(1),
        }
    }

    fn commands(deck: &RemoteDeck<FakeSsh>) -> Vec<String> {
        deck.port
            .executor_for_tests()
            .calls
            .borrow()
            .iter()
            .map(|(_, command)| command.clone())
            .collect()
    }

    fn failed(outcome: StartOutcome) -> StartProblem {
        match outcome {
            StartOutcome::Failed(problem) => problem,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    const SOCKET_ENV: &str = "env DOT_AGENT_DECK_ATTACH_SOCKET=/run/deck/attach.sock ";

    // -- the ssh invocation -------------------------------------------------

    /// Every command reaches the deck's machine by the route the deck carries
    /// — user, port, key and jump host — and runs with the deck's socket as
    /// `DOT_AGENT_DECK_ATTACH_SOCKET`, the variable `daemon serve` binds its
    /// attach endpoint from.
    #[test]
    fn the_remote_start_uses_the_decks_route_and_socket() {
        let fake = FakeSsh::default();
        fake.version.borrow_mut().push_back(version_ok());
        fake.probe
            .borrow_mut()
            .extend([probe_says(false), probe_says(true)]);
        fake.start.borrow_mut().push_back(out(0, "", ""));
        let deck = deck(fake);

        assert_eq!(start_remote(&deck, quick()), StartOutcome::Started);

        let calls = deck.port.executor_for_tests().calls.borrow().clone();
        for (target, _) in &calls {
            assert_eq!(target.host, "build-box");
            assert_eq!(target.user.as_deref(), Some("deploy"));
            assert_eq!(target.port, 2222);
            assert_eq!(
                target.key.as_deref(),
                Some(Path::new("/home/me/.ssh/id_deck"))
            );
            assert_eq!(target.jump.as_deref(), Some("bastion"));
        }
        assert_eq!(
            commands(&deck),
            [
                "~/.local/bin/dot-agent-deck --version".to_string(),
                format!("{SOCKET_ENV}~/.local/bin/dot-agent-deck daemon probe --json"),
                format!(
                    "{SOCKET_ENV}nohup ~/.local/bin/dot-agent-deck daemon serve </dev/null >/dev/null 2>&1 &"
                ),
                format!("{SOCKET_ENV}~/.local/bin/dot-agent-deck daemon probe --json"),
            ]
        );

        // And the real executor turns that route into ssh's own flags.
        let target = endpoint().ssh_target();
        let command = start_ssh_executor().build_command(&target, "x");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.contains("BatchMode=yes"), "{joined}");
        assert!(joined.contains("ConnectTimeout=10"), "{joined}");
        assert!(
            joined.contains("-p 2222 -i /home/me/.ssh/id_deck -J bastion -- deploy@build-box x"),
            "{joined}"
        );
    }

    /// Issue #1490 audit A1: the checks the desktop runs unattended and the
    /// detached start are observation sessions that require a trusted host
    /// key. Each ssh invocation the production executor makes — the probe,
    /// the older `daemon endpoint` check, the `--version` check and the
    /// detached start — is recorded by a stand-in `ssh` and must suppress every
    /// delegation, forwarding, local-command and shared-master option a user
    /// `Host` block could turn on, not only carry batch mode and the route.
    #[cfg(unix)]
    #[test]
    fn every_remote_check_and_the_start_delegate_no_credential() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv.log");
        let script = dir.path().join("ssh");
        crate::test_isolation::write_script(
            &script,
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done >> '{log}'\necho --- >> '{log}'\nexit 0\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let deck = RemoteDeck::with_executor(
            start_ssh_executor().with_program(&script),
            &endpoint(),
            None,
        );

        let _ = deck.port.probe();
        let _ = deck.port.endpoint();
        let _ = deck.port.version();
        let _ = deck.port.start_detached();

        let recorded = std::fs::read_to_string(&log).unwrap();
        let sessions: Vec<Vec<&str>> = recorded
            .split("---\n")
            .filter(|session| !session.is_empty())
            .map(|session| session.lines().collect())
            .collect();
        let remote_commands: Vec<&str> = sessions
            .iter()
            .map(|args| *args.last().expect("a remote command"))
            .collect();
        assert_eq!(sessions.len(), 4, "{remote_commands:?}");
        assert!(remote_commands[0].ends_with("daemon probe --json"));
        assert!(remote_commands[1].ends_with("daemon endpoint"));
        assert!(remote_commands[2].ends_with("--version"));
        assert!(
            remote_commands[3].contains("nohup ") && remote_commands[3].contains("daemon serve"),
            "{remote_commands:?}"
        );
        for args in &sessions {
            let options: Vec<&str> = args
                .windows(2)
                .filter(|pair| pair[0] == "-o")
                .map(|pair| pair[1])
                .collect();
            for expected in [
                "BatchMode=yes",
                "StrictHostKeyChecking=yes",
                "ForwardAgent=no",
                "ForwardX11=no",
                "ForwardX11Trusted=no",
                "GSSAPIDelegateCredentials=no",
                "AddKeysToAgent=no",
                "PermitLocalCommand=no",
                "ClearAllForwardings=yes",
                "ControlMaster=no",
                "ControlPath=none",
                "UpdateHostKeys=no",
            ] {
                assert!(
                    options.contains(&expected),
                    "`{}` must run with -o {expected}: {options:?}",
                    args.last().unwrap()
                );
            }
            // And no weaker spelling of them appears anywhere in the argv.
            for weaker in [
                "ForwardAgent=yes",
                "ForwardX11=yes",
                "StrictHostKeyChecking=no",
                "StrictHostKeyChecking=accept-new",
            ] {
                assert!(!options.contains(&weaker), "{options:?}");
            }
            assert!(
                args.ends_with(&["--", "deploy@build-box", args.last().unwrap()]),
                "{args:?}"
            );
        }
    }

    /// The deck-list row's binary (a Homebrew install) is the one every
    /// command runs.
    #[test]
    fn a_recorded_binary_is_the_one_run() {
        let fake = FakeSsh::default();
        fake.probe.borrow_mut().push_back(probe_says(false));
        let homebrew =
            RemoteBinaryPath::try_from("/opt/homebrew/bin/dot-agent-deck".to_string()).unwrap();
        let deck = RemoteDeck::with_executor(fake, &endpoint(), Some(&homebrew));
        assert_eq!(probe_remote(&deck), DisconnectedReason::NotRunning);
        assert_eq!(
            commands(&deck),
            [format!(
                "{SOCKET_ENV}/opt/homebrew/bin/dot-agent-deck daemon probe --json"
            )]
        );
    }

    // -- the remote start's outcomes ----------------------------------------

    #[test]
    fn an_already_running_daemon_is_not_started_again() {
        let fake = FakeSsh::default();
        fake.version.borrow_mut().push_back(version_ok());
        fake.probe.borrow_mut().push_back(probe_says(true));
        let deck = deck(fake);
        assert_eq!(start_remote(&deck, quick()), StartOutcome::AlreadyRunning);
        assert!(
            commands(&deck).iter().all(|c| !c.contains("daemon serve")),
            "nothing may be started beside a running daemon"
        );
    }

    #[test]
    fn an_unreachable_host_is_reported_as_unreachable() {
        let fake = FakeSsh::default();
        fake.version.borrow_mut().push_back(unreachable());
        let problem = failed(start_remote(&deck(fake), quick()));
        assert_eq!(problem.failure, StartFailure::HostUnreachable);
        assert_eq!(
            problem.message,
            "The app cannot reach deploy@build-box:2222 over ssh. Check that the host is up and reachable from this machine."
        );
        assert!(problem.detail.unwrap().contains("Connection timed out"));
    }

    #[test]
    fn a_refused_login_is_reported_as_an_authentication_failure() {
        let fake = FakeSsh::default();
        fake.version
            .borrow_mut()
            .push_back(Err(SshError::AuthFailed {
                target: "deploy@build-box".into(),
                detail: "Permission denied (publickey).".into(),
            }));
        let problem = failed(start_remote(&deck(fake), quick()));
        assert_eq!(problem.failure, StartFailure::AuthFailed);
        assert_eq!(
            problem.message,
            "ssh could not log in to deploy@build-box:2222. Check the key in this deck's settings, or your ~/.ssh/config."
        );
    }

    #[test]
    fn an_untrusted_host_key_names_the_command_that_trusts_it() {
        let fake = FakeSsh::default();
        fake.version
            .borrow_mut()
            .push_back(Err(SshError::HostKeyVerificationFailed {
                target: "deploy@build-box".into(),
                remedy: "ssh -p 2222 -J bastion deploy@build-box".into(),
            }));
        let problem = failed(start_remote(&deck(fake), quick()));
        assert_eq!(problem.failure, StartFailure::HostKeyNotTrusted);
        assert!(
            problem
                .message
                .contains("Run `ssh -p 2222 -J bastion deploy@build-box`"),
            "{}",
            problem.message
        );
    }

    #[test]
    fn a_missing_install_points_at_remote_add() {
        for answer in [
            out(127, "", "sh: ~/.local/bin/dot-agent-deck: not found"),
            out(0, "hello from a stub\n", ""),
        ] {
            let fake = FakeSsh::default();
            fake.version.borrow_mut().push_back(answer);
            let deck = deck(fake);
            let problem = failed(start_remote(&deck, quick()));
            assert_eq!(problem.failure, StartFailure::NotInstalled);
            assert_eq!(
                problem.message,
                "dot-agent-deck is not installed on deploy@build-box:2222. Install it there with `dot-agent-deck remote add`."
            );
            assert_eq!(commands(&deck).len(), 1, "nothing runs after the probe");
        }
    }

    #[test]
    fn a_daemon_that_never_answers_names_the_configured_socket() {
        let fake = FakeSsh::default();
        fake.version.borrow_mut().push_back(version_ok());
        fake.probe.borrow_mut().push_back(probe_says(false));
        fake.start.borrow_mut().push_back(out(0, "", ""));
        let problem = failed(start_remote(&deck(fake), quick()));
        assert_eq!(problem.failure, StartFailure::DidNotAnswer);
        assert!(
            problem.message.starts_with(
                "The daemon was started on deploy@build-box:2222 but did not answer at /run/deck/attach.sock"
            ),
            "{}",
            problem.message
        );
    }

    #[test]
    fn a_failed_detached_start_is_a_start_failure() {
        let fake = FakeSsh::default();
        fake.version.borrow_mut().push_back(version_ok());
        fake.probe.borrow_mut().push_back(probe_says(false));
        fake.start.borrow_mut().push_back(out(
            1,
            "",
            "sh: cannot create /dev/null: Permission denied",
        ));
        let problem = failed(start_remote(&deck(fake), quick()));
        assert_eq!(problem.failure, StartFailure::StartFailed);
        assert_eq!(
            problem.message,
            "Could not start the daemon on deploy@build-box:2222."
        );
    }

    // -- the disconnected reason, remote ------------------------------------

    #[test]
    fn the_remote_reason_distinguishes_the_three_states() {
        let fake = FakeSsh::default();
        fake.probe.borrow_mut().push_back(probe_says(false));
        let reason = probe_remote(&deck(fake));
        assert_eq!(reason, DisconnectedReason::NotRunning);
        assert_eq!(reason.action(), DisconnectedAction::StartDaemon);

        let fake = FakeSsh::default();
        fake.probe.borrow_mut().push_back(probe_says(true));
        let reason = probe_remote(&deck(fake));
        assert_eq!(reason, DisconnectedReason::RunningNotConnected);
        assert_eq!(reason.action(), DisconnectedAction::Reconnect);

        let fake = FakeSsh::default();
        fake.probe.borrow_mut().push_back(unreachable());
        let reason = probe_remote(&deck(fake));
        let DisconnectedReason::Unknown(problem) = &reason else {
            panic!("an unreachable host cannot tell: {reason:?}");
        };
        assert_eq!(problem.failure, StartFailure::HostUnreachable);
        assert_eq!(reason.action(), DisconnectedAction::Reconnect);
        assert_eq!(reason.summary("ignored"), problem.message);
    }

    /// A deck from before `daemon probe` answers through `daemon endpoint`'s
    /// exit codes instead.
    #[test]
    fn an_older_deck_is_asked_through_daemon_endpoint() {
        let usage = || out(2, "", "error: unrecognized subcommand 'probe'");
        let cases = [
            (
                out(0, "/run/deck/attach.sock\n", ""),
                DisconnectedReason::RunningNotConnected,
            ),
            (
                out(
                    3,
                    "",
                    "daemon endpoint: nothing at /run/deck/attach.sock: No such file or directory (os error 2)",
                ),
                DisconnectedReason::NotRunning,
            ),
            (
                out(
                    3,
                    "",
                    "daemon endpoint: nothing usable answered at /run/x: Connection refused (os error 111)",
                ),
                DisconnectedReason::NotRunning,
            ),
            (
                out(
                    3,
                    "",
                    "daemon endpoint: nothing usable answered at /run/x: daemon speaks attach protocol v7",
                ),
                DisconnectedReason::RunningNotConnected,
            ),
        ];
        for (answer, expected) in cases {
            let fake = FakeSsh::default();
            fake.probe.borrow_mut().push_back(usage());
            fake.endpoint.borrow_mut().push_back(answer);
            assert_eq!(probe_remote(&deck(fake)), expected);
        }

        // Older still: neither subcommand exists.
        let fake = FakeSsh::default();
        fake.probe.borrow_mut().push_back(usage());
        fake.endpoint.borrow_mut().push_back(usage());
        let DisconnectedReason::Unknown(problem) = probe_remote(&deck(fake)) else {
            panic!("a deck too old to say cannot tell");
        };
        assert_eq!(problem.failure, StartFailure::TooOld);
        assert!(problem.message.contains("dot-agent-deck remote upgrade"));
    }

    #[test]
    fn a_missing_install_cannot_tell_and_says_why() {
        let fake = FakeSsh::default();
        fake.probe.borrow_mut().push_back(out(
            127,
            "",
            "sh: 1: ~/.local/bin/dot-agent-deck: not found",
        ));
        let DisconnectedReason::Unknown(problem) = probe_remote(&deck(fake)) else {
            panic!("a missing install cannot tell");
        };
        assert_eq!(problem.failure, StartFailure::NotInstalled);
    }

    // -- the disconnected reason and start, local ---------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn the_local_reason_distinguishes_nothing_from_a_listener() {
        let root = tempfile::tempdir().unwrap();

        let absent = LocalEndpoint::at(root.path().join("absent.sock"));
        assert_eq!(probe_local(&absent).await, DisconnectedReason::NotRunning);

        // A daemon that died without unlinking its socket: the inode is there
        // and refuses every connection.
        let stale = root.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        assert_eq!(
            probe_local(&LocalEndpoint::at(&stale)).await,
            DisconnectedReason::NotRunning
        );

        let live = root.path().join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        let reason = probe_local(&LocalEndpoint::at(&live)).await;
        assert_eq!(reason, DisconnectedReason::RunningNotConnected);
        assert_eq!(reason.action(), DisconnectedAction::Reconnect);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_local_start_spawns_only_when_nothing_runs() {
        use std::sync::{Arc, Mutex};
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let socket = root.path().join("attach.sock");
        let holder = Arc::new(Mutex::new(None));
        let (for_spawn, holder_for_spawn) = (socket.clone(), Arc::clone(&holder));
        let outcome = start_local(
            &LocalEndpoint::at(&socket),
            &state,
            move || {
                let listener = std::os::unix::net::UnixListener::bind(&for_spawn)?;
                crate::platform::fsperm::set_endpoint_mode_owner_only(&for_spawn)?;
                *holder_for_spawn.lock().unwrap() = Some(listener);
                Ok(())
            },
            Duration::from_millis(5),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(outcome, StartOutcome::Started);

        let outcome = start_local(
            &LocalEndpoint::at(&socket),
            &state,
            || panic!("a running daemon must not be started again"),
            Duration::from_millis(5),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(outcome, StartOutcome::AlreadyRunning);
        drop(holder);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_local_daemon_that_never_binds_did_not_answer() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("attach.sock");
        let problem = failed(
            start_local(
                &LocalEndpoint::at(&socket),
                &root.path().join("state"),
                || Ok(()),
                Duration::from_millis(5),
                Duration::from_millis(30),
            )
            .await,
        );
        assert_eq!(problem.failure, StartFailure::DidNotAnswer);
        assert!(
            problem.message.contains("this machine"),
            "{}",
            problem.message
        );
    }

    // -- shapes -------------------------------------------------------------

    #[test]
    fn the_reason_and_outcome_serialize_with_a_kind_tag() {
        assert_eq!(
            serde_json::to_value(DisconnectedReason::NotRunning).unwrap(),
            serde_json::json!({ "kind": "not-running" })
        );
        assert_eq!(
            serde_json::to_value(DisconnectedReason::Unknown(StartProblem::new(
                StartFailure::HostUnreachable,
                "m".into(),
                Some("d".into())
            )))
            .unwrap(),
            serde_json::json!({ "kind": "unknown", "failure": "host-unreachable", "message": "m", "detail": "d" })
        );
        assert_eq!(
            serde_json::to_value(StartOutcome::AlreadyRunning).unwrap(),
            serde_json::json!({ "outcome": "already-running" })
        );
        assert_eq!(
            serde_json::to_value(StartOutcome::Failed(StartProblem::new(
                StartFailure::NotInstalled,
                "m".into(),
                None
            )))
            .unwrap(),
            serde_json::json!({ "outcome": "failed", "failure": "not-installed", "message": "m" })
        );
    }

    #[test]
    fn the_host_is_named_as_the_user_reads_it() {
        assert_eq!(
            host_label(&Endpoint::Local(LocalEndpoint::at("/tmp/x.sock"))),
            "this machine"
        );
        assert_eq!(
            host_label(&Endpoint::Remote(endpoint())),
            "deploy@build-box:2222"
        );
    }
}
