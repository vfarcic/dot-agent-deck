//! Upgrade a deck's daemon: install the new build, then restart the daemon
//! onto it under the daemon's own live-agent policy (PRD #1487).
//!
//! One function, [`upgrade_daemon`], runs the whole sequence for every client
//! — `remote upgrade`, `connect`'s upgrade nudge, and the desktop's Upgrade and
//! Replace daemon actions — over three seams:
//!
//! - an [`Installer`] puts the build in place ([`SshInstaller`] over ssh;
//!   [`NoInstall`] for the local Replace, whose build is the client's own);
//! - a [`DaemonPort`] reaches the running daemon ([`SshDaemonPort`] through the
//!   remote's freshly installed binary; [`WireDaemonPort`] over the local
//!   socket);
//! - a [`RestartDecider`] answers the one question the daemon may ask — "these
//!   agents and roles would stop; restart now?" — or says no one can
//!   ([`NoDecider`]).
//!
//! The daemon decides whether a question is needed at all: an idle daemon
//! restarts straight away, with or without anyone to ask, and a daemon with
//! live agents or orchestration roles answers `NeedsConfirmation` with every
//! one of them named. The result is one serde type, [`UpgradeOutcome`], with a
//! plain-language [`UpgradeOutcome::summary`].
//!
//! Sync on purpose: the ssh calls and the CLI/connect callers are blocking. The
//! desktop calls it inside `spawn_blocking`; [`WireDaemonPort`] bridges to the
//! async client with a held runtime handle, which is legal on a blocking thread.

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::daemon_client::{
    ClientError, DaemonClient, Endpoint, GatedQuery, LocalEndpoint, RestartDaemonRequest,
};
use crate::daemon_protocol::{
    AttachResponse, RestartDaemonReply, RestartRefusalReason, RestartStopSet, RestartSuccessor,
};
use crate::remote::{RemoteEntry, SshError, SshExecutor, SystemSshExecutor};
use crate::remote_daemon::{RemoteDaemonError, SshDaemonPort};
use crate::untrusted_text::{
    REMOTE_MESSAGE_MAX_BYTES, REMOTE_NAME_MAX_BYTES, REMOTE_PATH_MAX_BYTES, display_line,
    display_message,
};

// ---------------------------------------------------------------------------
// Result and progress types
// ---------------------------------------------------------------------------

/// What an upgrade did. Every arm but [`Self::Failed`] is a successful run of
/// the command, even when the daemon was left running — that is the user's
/// choice or the daemon's policy, not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum UpgradeOutcome {
    /// The new build is installed and the daemon now runs it. `stopped` is
    /// what the restart stopped (empty for an idle daemon).
    Restarted {
        from_version: String,
        to_version: String,
        stopped: RestartStopSet,
    },
    /// The new build is installed; the daemon that was running keeps running.
    InstalledNotRestarted {
        from_version: Option<String>,
        installed_version: String,
        reason: NotRestartedReason,
    },
    /// The new build is installed, but the running daemon predates the restart
    /// request, so it cannot be asked to restart itself.
    InstalledDaemonTooOld {
        installed_version: String,
        daemon_version: Option<String>,
        remedy: String,
    },
    /// A stage failed. `installed_version` is set when the install had already
    /// succeeded.
    ///
    /// `old_daemon_gone` says whether the daemon that was asked may no longer
    /// be serving what it had: it accepted the restart (and so stopped its
    /// agents), or it was stopped from outside, or its reply was lost and
    /// afterwards the endpoint did not answer as that daemon. `false` when it
    /// was never asked, refused, or is known to be the one still answering. A
    /// client holding that daemon's terminal sessions drops them when this is
    /// set, as it does after [`Self::Restarted`] (Qodo 4200693875).
    Failed {
        stage: UpgradeStage,
        reason: String,
        installed_version: Option<String>,
        #[serde(default)]
        old_daemon_gone: bool,
    },
}

/// Why [`UpgradeOutcome::InstalledNotRestarted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NotRestartedReason {
    /// The user chose "Keep current daemon".
    KeptByUser { at_stake: RestartStopSet },
    /// Agents or roles would stop and there was no one to ask (no terminal).
    NoOneToAsk { at_stake: RestartStopSet },
    /// What would stop kept changing while the user was being asked.
    StaleConfirmation { at_stake: RestartStopSet },
    /// Another client is already restarting this daemon.
    AnotherRestartInProgress,
    /// No daemon was running; the next one to start runs the new build.
    NoDaemonRunning,
    /// The installed build predates the commands that reach the running
    /// daemon (an older release named with `--version`, or an older Homebrew
    /// tap release), so it could not ask the daemon anything.
    InstalledBuildTooOld,
    /// The local daemon predates the restart request, so it can only be
    /// stopped from outside — and that is done only while nothing runs on it.
    /// These were running when it was checked, so it was not stopped.
    OlderDaemonBusy { at_stake: RestartStopSet },
}

/// The stage an upgrade is in, for progress and for [`UpgradeOutcome::Failed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeStage {
    Installing,
    Restarting,
    Verifying,
}

/// One progress report from [`upgrade_daemon`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeProgress {
    pub stage: UpgradeStage,
    #[serde(default)]
    pub detail: Option<String>,
}

/// The display copy of a remote-reported version (audit A3).
fn shown_version(raw: &str) -> String {
    display_line(raw, REMOTE_NAME_MAX_BYTES)
}

/// `text` with its first character upper-cased, to open a sentence.
fn sentence_start(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl UpgradeOutcome {
    /// Whether the daemon that was running before the upgrade may be gone, so
    /// what a client held from it — its terminal sessions — is stale: after a
    /// restart, and after a failure that came after it accepted or may have
    /// ([`Self::Failed`]'s `old_daemon_gone`).
    pub fn old_daemon_gone(&self) -> bool {
        match self {
            Self::Restarted { .. } => true,
            Self::Failed {
                old_daemon_gone, ..
            } => *old_daemon_gone,
            Self::InstalledNotRestarted { .. } | Self::InstalledDaemonTooOld { .. } => false,
        }
    }

    /// Plain-language rendering for a person; the CLI prints it. Clients with
    /// their own surface may build their own from the fields instead.
    ///
    /// Every remote-reported field — versions, names, paths, a refusal or
    /// failure reason — is shown through [`crate::untrusted_text`]'s display
    /// sanitizers, so a hostile or broken remote cannot steer the terminal it
    /// is printed to (PRD #1487 audit A3). The outcome itself keeps the raw
    /// values.
    pub fn summary(&self, deck: &str) -> String {
        match self {
            Self::Restarted {
                from_version,
                to_version,
                stopped,
            } => {
                let from_version = shown_version(from_version);
                let to_version = shown_version(to_version);
                let mut text = format!(
                    "Restarted the daemon on '{deck}' onto the new build (was {from_version}, now {to_version})."
                );
                if !stopped.is_empty() {
                    text.push_str("\nStopped:\n");
                    text.push_str(&describe_stop_set(stopped));
                }
                text
            }
            Self::InstalledNotRestarted {
                from_version,
                installed_version,
                reason,
            } => {
                let installed_version = &shown_version(installed_version);
                let running = from_version
                    .as_deref()
                    .map(|v| format!(" It keeps running {}.", shown_version(v)))
                    .unwrap_or_default();
                match reason {
                    NotRestartedReason::KeptByUser { at_stake } => format!(
                        "Installed {installed_version} on '{deck}'. The daemon was not restarted: you chose to keep it running with:\n{}{running} It switches to the new build the next time it restarts.",
                        describe_stop_set(at_stake)
                    ),
                    NotRestartedReason::NoOneToAsk { at_stake } => format!(
                        "Installed {installed_version} on '{deck}'. The daemon was not restarted, because restarting it would stop the following and no one was at a terminal to confirm:\n{}{running} Run `dot-agent-deck remote upgrade {deck}` in a terminal to choose, or restart the daemon once that work is finished.",
                        describe_stop_set(at_stake)
                    ),
                    NotRestartedReason::StaleConfirmation { at_stake } => format!(
                        "Installed {installed_version} on '{deck}'. The daemon was not restarted, because what was running kept changing while you were asked. Running now:\n{}{running} Try again when it settles.",
                        describe_stop_set(at_stake)
                    ),
                    NotRestartedReason::AnotherRestartInProgress => format!(
                        "Installed {installed_version} on '{deck}'. The daemon was not restarted here because another restart of it is already in progress."
                    ),
                    NotRestartedReason::NoDaemonRunning => format!(
                        "Installed {installed_version} on '{deck}'. No daemon was running, so nothing was restarted; the next one to start runs the new build."
                    ),
                    NotRestartedReason::InstalledBuildTooOld => format!(
                        "Installed {installed_version} on '{deck}'. The daemon was not restarted, because {installed_version} is too old to restart it from here; the daemon that was running keeps running. {}",
                        installed_too_old_remedy(deck, installed_version)
                    ),
                    NotRestartedReason::OlderDaemonBusy { at_stake } => format!(
                        "The daemon on '{deck}' was not replaced: it is too old to restart itself, and it is replaced only when nothing is running on it. Running on it:\n{}{running} Stop them, or let them finish, then try again.",
                        describe_stop_set(at_stake)
                    ),
                }
            }
            Self::InstalledDaemonTooOld {
                installed_version,
                daemon_version,
                remedy,
            } => {
                let installed_version = shown_version(installed_version);
                let remedy = display_message(remedy, REMOTE_MESSAGE_MAX_BYTES);
                let daemon = daemon_version
                    .as_deref()
                    .map(|v| format!(" ({})", shown_version(v)))
                    .unwrap_or_default();
                format!(
                    "Installed {installed_version} on '{deck}', but the running daemon{daemon} is too old to restart itself, so it keeps running. {remedy}"
                )
            }
            Self::Failed {
                stage,
                reason,
                installed_version,
                old_daemon_gone,
            } => {
                let doing = match stage {
                    UpgradeStage::Installing => "installing the new build",
                    UpgradeStage::Restarting => "restarting the daemon",
                    UpgradeStage::Verifying => "checking the restarted daemon",
                };
                let reason = display_message(reason, REMOTE_MESSAGE_MAX_BYTES);
                // Starts a sentence below, and may be prose rather than a
                // version ("an unverified build", when the check after the
                // binary was replaced read none).
                let installed_version = installed_version
                    .as_deref()
                    .map(|v| sentence_start(&shown_version(v)));
                let mut text = format!("Upgrade of '{deck}' failed while {doing}: {reason}");
                match (stage, installed_version) {
                    (UpgradeStage::Verifying, Some(v)) => text.push_str(&format!(
                        "\n{v} is installed; the daemon was asked to restart onto it, and the next one to start runs it."
                    )),
                    (UpgradeStage::Installing, Some(v)) => text.push_str(&format!(
                        "\n{v} is installed, but the upgrade stopped before restarting the daemon, so the daemon that was running keeps running. Run `dot-agent-deck remote upgrade {deck}` again to finish."
                    )),
                    // Qodo 4201244671: a daemon that may have stopped is
                    // never said to keep running.
                    (_, Some(v)) if *old_daemon_gone => text.push_str(&format!(
                        "\n{v} is installed. The daemon that was running may have stopped; `dot-agent-deck connect {deck}` shows what is answering now."
                    )),
                    (_, None) if *old_daemon_gone => text.push_str(&format!(
                        "\nThe daemon that was running may have stopped; `dot-agent-deck connect {deck}` shows what is answering now."
                    )),
                    (_, Some(v)) => text.push_str(&format!(
                        "\n{v} is installed; the daemon that was running keeps running."
                    )),
                    (_, None) => {
                        text.push_str("\nThe daemon that was running keeps running.")
                    }
                }
                text
            }
        }
    }

    /// Whether the CLI should exit non-zero: only [`Self::Failed`].
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// The agents and roles in `set`, one per line, indented — what the restart
/// question and the summaries name.
///
/// Every field is remote-supplied (a remote daemon's stop set arrives over ssh
/// as JSON, and a working directory is whatever the filesystem holds), so each
/// is shown through [`display_line`]: no control or bidi character, no line
/// break of its own, and a bounded length (PRD #1487 audit A3). This is the
/// display copy only — the confirmation sent back is the set as received.
pub fn describe_stop_set(set: &RestartStopSet) -> String {
    let name = |raw: &str| display_line(raw, REMOTE_NAME_MAX_BYTES);
    let mut text = String::new();
    if !set.agents.is_empty() {
        text.push_str("  Agents:\n");
        for agent in &set.agents {
            let label = name(&agent.label);
            let mut place = Vec::new();
            if let Some(pane) = &agent.pane_id {
                place.push(format!("pane {}", name(pane)));
            }
            if let Some(cwd) = &agent.cwd {
                place.push(format!("in {}", display_line(cwd, REMOTE_PATH_MAX_BYTES)));
            }
            if place.is_empty() {
                text.push_str(&format!("    {label}\n"));
            } else {
                text.push_str(&format!("    {label} ({})\n", place.join(", ")));
            }
        }
    }
    if !set.roles.is_empty() {
        text.push_str("  Orchestration roles:\n");
        for role in &set.roles {
            let lead = if role.is_orchestrator {
                " (orchestrator)"
            } else {
                ""
            };
            text.push_str(&format!(
                "    {}: {} in pane {}{lead}\n",
                name(&role.orchestration),
                name(&role.role),
                name(&role.pane_id)
            ));
        }
    }
    text
}

// ---------------------------------------------------------------------------
// The seams
// ---------------------------------------------------------------------------

/// What to upgrade to, and who starts the new daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePlan {
    /// The version to install: the client's own (PRD #1487 D11) unless the
    /// user named another.
    pub version: String,
    /// [`RestartSuccessor::Installed`] for a remote upgrade;
    /// [`RestartSuccessor::ClientSpawns`] for the local Replace.
    pub successor: RestartSuccessor,
}

/// How the build got onto the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// Downloaded to `~/.local/bin`.
    LocalBin,
    /// `brew upgrade` (what landed may differ from the version asked for).
    Homebrew,
    /// Nothing installed: the local Replace runs the client's own build.
    ClientBuild,
}

/// What an [`Installer`] put in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBuild {
    /// The version the installed binary reports.
    pub version: String,
    pub method: InstallMethod,
    /// The installed binary, spelled for the machine's shell.
    pub binary: String,
}

/// Why an [`Installer`] did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallError {
    /// In plain language, naming the step that failed.
    pub reason: String,
    /// Set when the new build was already in place and a step after it (the
    /// hooks, the deck list) failed — the build is not rolled back. Also set
    /// when the binary had been replaced and the version check after it
    /// failed: then it is the version that check read, or
    /// [`crate::remote::UNVERIFIED_BUILD`] when it read none.
    pub installed_version: Option<String>,
}

impl From<String> for InstallError {
    fn from(reason: String) -> Self {
        Self {
            reason,
            installed_version: None,
        }
    }
}

/// Puts a build in place.
pub trait Installer {
    fn install(&self, version: &str) -> Result<InstalledBuild, InstallError>;
}

/// Why a [`DaemonPort`] call — the probe or the restart request — got no
/// answer from the running daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    /// The installed build cannot reach the running daemon: it predates the
    /// command that asks (the probe, or the restart request — PRD #1487 review
    /// S1, so the restart path no longer blames the daemon). Not a failure of the upgrade — the build is in
    /// place — so it becomes [`NotRestartedReason::InstalledBuildTooOld`].
    InstalledBuildTooOld(String),
    /// The restart request ran, but no usable reply came back: none arrived
    /// (the session dropped, the deadline passed, the output was empty), or
    /// what arrived could not be parsed (cut off, oversized, or not this
    /// build's JSON). The daemon may or may not have restarted, so the upgrade
    /// checks before it says which.
    ReplyUnreadable(String),
    /// No daemon was running to ask: it exited after the probe found it and
    /// before the restart request reached it (Qodo 4202060284). Not a failure
    /// of the upgrade — the build is installed and the next start runs it — so
    /// it becomes [`NotRestartedReason::NoDaemonRunning`].
    NoDaemonRunning,
    /// Anything else, in plain language.
    Other(String),
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InstalledBuildTooOld(reason)
            | Self::ReplyUnreadable(reason)
            | Self::Other(reason) => f.write_str(reason),
            Self::NoDaemonRunning => f.write_str("no daemon is running"),
        }
    }
}

impl From<String> for PortError {
    fn from(reason: String) -> Self {
        Self::Other(reason)
    }
}

impl From<&str> for PortError {
    fn from(reason: &str) -> Self {
        Self::Other(reason.to_string())
    }
}

/// Reaches the running daemon.
pub trait DaemonPort {
    /// The running daemon's `Hello`, or `Ok(None)` when none is running. Never
    /// starts one.
    fn probe(&self) -> Result<Option<AttachResponse>, PortError>;
    /// [`Self::probe`], given up at `budget` when that comes before the port's
    /// own bound on one probe — what the verify wait uses so a slow probe
    /// cannot carry it past its deadline (PRD #1487, Qodo 4202262493). The
    /// default ignores `budget`; a port whose probe can block overrides it.
    fn probe_within(&self, budget: Duration) -> Result<Option<AttachResponse>, PortError> {
        let _ = budget;
        self.probe()
    }
    /// The shortest budget [`Self::probe_within`] can honour. The verify wait
    /// starts no probe with less than this left, since the port would either
    /// refuse it or run it past the budget. Zero by default.
    fn min_probe_budget(&self) -> Duration {
        Duration::ZERO
    }
    /// Send the restart request. Must go through
    /// [`DaemonClient::restart_daemon`], which withholds it from a daemon that
    /// does not advertise it — `Unsupported` then means the daemon is too old.
    /// [`PortError::InstalledBuildTooOld`] means the build that would send it
    /// is too old instead.
    fn restart(
        &self,
        req: &RestartDaemonRequest,
    ) -> Result<GatedQuery<RestartDaemonReply>, PortError>;
    /// Told what the installer put in place, before the first probe — a port
    /// that runs the installed binary repoints itself here.
    fn installed(&self, _build: &InstalledBuild) {}
    /// Start the client's own build after the old daemon has gone. Only called
    /// for [`RestartSuccessor::ClientSpawns`]; it returns once the old daemon
    /// has released its endpoint and the new one has been started.
    fn spawn_successor(&self) -> Result<(), String> {
        Ok(())
    }
    /// A fallback for a daemon without the restart request. `None` (the
    /// default, and every remote) gives [`UpgradeOutcome::InstalledDaemonTooOld`].
    /// It asks no one: such a daemon cannot hold new starts while a question
    /// is open, so it is restarted only when idle.
    fn legacy_restart(&self) -> Option<UpgradeOutcome> {
        None
    }
}

/// The answer to "these would stop; restart now?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartChoice {
    RestartNow,
    KeepCurrent,
    /// No one can answer (no terminal, no dialog).
    NoOneToAsk,
}

/// Answers the restart question. Called only when the daemon answered
/// `NeedsConfirmation` — an idle daemon restarts without asking anyone.
pub trait RestartDecider {
    /// `stale` means what would stop changed since the last time this asked.
    fn decide(&self, deck: &str, at_stake: &RestartStopSet, stale: bool) -> RestartChoice;
}

/// There is no one to ask: always [`RestartChoice::NoOneToAsk`] (D6).
pub struct NoDecider;

impl RestartDecider for NoDecider {
    fn decide(&self, _deck: &str, _at_stake: &RestartStopSet, _stale: bool) -> RestartChoice {
        RestartChoice::NoOneToAsk
    }
}

/// Asks on a terminal. Default and end of input are "keep".
pub struct TtyDecider<R: BufRead, W: Write> {
    input: RefCell<R>,
    output: RefCell<W>,
}

impl<R: BufRead, W: Write> TtyDecider<R, W> {
    pub fn new(input: R, output: W) -> Self {
        Self {
            input: RefCell::new(input),
            output: RefCell::new(output),
        }
    }
}

impl<R: BufRead, W: Write> RestartDecider for TtyDecider<R, W> {
    fn decide(&self, deck: &str, at_stake: &RestartStopSet, stale: bool) -> RestartChoice {
        let mut out = self.output.borrow_mut();
        let changed = if stale {
            format!("What is running on '{deck}' changed since you were asked.\n")
        } else {
            String::new()
        };
        let asked = write!(
            out,
            "{changed}Restarting the daemon on '{deck}' onto the new build stops:\n{}Restart now? [r] / Keep current daemon [K]: ",
            describe_stop_set(at_stake)
        )
        .and_then(|()| out.flush());
        if asked.is_err() {
            return RestartChoice::KeepCurrent;
        }
        let mut line = String::new();
        match self.input.borrow_mut().read_line(&mut line) {
            Ok(n) if n > 0 => {
                let answer = line.trim();
                if answer.eq_ignore_ascii_case("r") || answer.eq_ignore_ascii_case("restart") {
                    RestartChoice::RestartNow
                } else {
                    RestartChoice::KeepCurrent
                }
            }
            _ => {
                // End of input: no answer is "keep", and the cursor moves on.
                let _ = writeln!(out);
                RestartChoice::KeepCurrent
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The shared function
// ---------------------------------------------------------------------------

/// How many restart requests one upgrade sends before giving up on a stop set
/// that keeps changing under the user's answer.
pub const MAX_CONFIRM_ROUNDS: usize = 3;
/// How long to wait for the restarted daemon to answer with the new build.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
/// How often to ask while waiting.
pub const VERIFY_POLL: Duration = Duration::from_millis(250);
/// The fallback for an old daemon that does not name its process
/// ([`AttachResponse::instance_id`], which predates it). When the new build is
/// the same build as the old (a reinstall), its answer cannot be told from the
/// old daemon's. If the endpoint was never seen empty, a matching answer is
/// accepted once it has held this long — longer than the old daemon's whole
/// drain (3s) plus margin, so it is taken not to be the old daemon.
pub const VERIFY_SETTLE: Duration = Duration::from_secs(6);

/// The waits [`upgrade_daemon`] uses, injectable so unit tests do not sleep.
#[derive(Debug, Clone, Copy)]
struct Timing {
    timeout: Duration,
    poll: Duration,
    settle: Duration,
}

const PRODUCTION_TIMING: Timing = Timing {
    timeout: VERIFY_TIMEOUT,
    poll: VERIFY_POLL,
    settle: VERIFY_SETTLE,
};

/// Install the plan's build, restart the daemon onto it under the daemon's
/// live-agent policy, and report what happened. Never panics on a failed
/// stage: every failure is an [`UpgradeOutcome::Failed`] naming the stage.
///
/// 1. `Installing`: `installer.install`.
/// 2. `Restarting`: probe the daemon (none running is
///    [`NotRestartedReason::NoDaemonRunning`]), then send the restart request,
///    asking `decider` whenever the daemon names live work, at most
///    [`MAX_CONFIRM_ROUNDS`] requests in all. A daemon without the request
///    falls back to [`DaemonPort::legacy_restart`] or is
///    [`UpgradeOutcome::InstalledDaemonTooOld`].
/// 3. For [`RestartSuccessor::ClientSpawns`], start the client's build.
/// 4. `Verifying`: wait up to [`VERIFY_TIMEOUT`] for the new daemon to answer.
pub fn upgrade_daemon(
    deck: &str,
    plan: &UpgradePlan,
    installer: &dyn Installer,
    daemon: &dyn DaemonPort,
    decider: &dyn RestartDecider,
    progress: &mut dyn FnMut(UpgradeProgress),
) -> UpgradeOutcome {
    run_upgrade(
        deck,
        plan,
        installer,
        daemon,
        decider,
        progress,
        PRODUCTION_TIMING,
    )
}

fn run_upgrade(
    deck: &str,
    plan: &UpgradePlan,
    installer: &dyn Installer,
    daemon: &dyn DaemonPort,
    decider: &dyn RestartDecider,
    progress: &mut dyn FnMut(UpgradeProgress),
    timing: Timing,
) -> UpgradeOutcome {
    progress(UpgradeProgress {
        stage: UpgradeStage::Installing,
        detail: Some(format!("installing {}", plan.version)),
    });
    let installed = match installer.install(&plan.version) {
        Ok(build) => build,
        Err(InstallError {
            reason,
            installed_version,
        }) => {
            return UpgradeOutcome::Failed {
                stage: UpgradeStage::Installing,
                reason,
                installed_version,
                old_daemon_gone: false,
            };
        }
    };
    daemon.installed(&installed);
    let failed_at =
        |stage: UpgradeStage, reason: String, old_daemon_gone: bool| UpgradeOutcome::Failed {
            stage,
            reason,
            installed_version: Some(installed.version.clone()),
            old_daemon_gone,
        };
    // Before the daemon accepted: it refused, or was never reached.
    let restarting_failed = |reason: String| failed_at(UpgradeStage::Restarting, reason, false);

    progress(UpgradeProgress {
        stage: UpgradeStage::Restarting,
        detail: None,
    });
    let hello = match daemon.probe() {
        Ok(Some(hello)) => hello,
        Ok(None) | Err(PortError::NoDaemonRunning) => {
            return UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: installed.version,
                reason: NotRestartedReason::NoDaemonRunning,
            };
        }
        Err(PortError::InstalledBuildTooOld(_)) => {
            return UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: installed.version,
                reason: NotRestartedReason::InstalledBuildTooOld,
            };
        }
        Err(PortError::Other(reason) | PortError::ReplyUnreadable(reason)) => {
            return restarting_failed(reason);
        }
    };
    let from_version = hello.daemon_version.clone();
    let from = OldDaemon {
        build: hello.build_version.clone(),
        instance: hello.instance_id.clone(),
    };
    let not_restarted = |reason: NotRestartedReason| UpgradeOutcome::InstalledNotRestarted {
        from_version: from_version.clone(),
        installed_version: installed.version.clone(),
        reason,
    };

    let mut confirm: Option<RestartStopSet> = None;
    let mut stopped = None;
    // Why the last request's answer was never read, when it was not.
    let mut reply_lost: Option<String> = None;
    for round in 0..MAX_CONFIRM_ROUNDS {
        let request = RestartDaemonRequest {
            confirm: confirm.clone(),
            expected_version: Some(installed.version.clone()),
            successor: plan.successor,
        };
        match daemon.restart(&request) {
            Err(PortError::InstalledBuildTooOld(_)) => {
                return not_restarted(NotRestartedReason::InstalledBuildTooOld);
            }
            Err(PortError::Other(reason)) => return restarting_failed(reason),
            // The daemon exited between the probe and the request: nothing was
            // stopped by this upgrade, and the next start runs the new build.
            Err(PortError::NoDaemonRunning) => {
                return not_restarted(NotRestartedReason::NoDaemonRunning);
            }
            // The request may have been carried out: the tail says so only if
            // a successor now answers. Under the daemon's policy it accepted
            // only an idle daemon or the set it was asked to confirm.
            Err(PortError::ReplyUnreadable(reason)) => {
                reply_lost = Some(reason);
                stopped = Some(confirm.clone().unwrap_or_default());
                break;
            }
            Ok(GatedQuery::Unsupported) => {
                return daemon.legacy_restart().unwrap_or_else(|| {
                    UpgradeOutcome::InstalledDaemonTooOld {
                        installed_version: installed.version.clone(),
                        daemon_version: from_version.clone(),
                        remedy: too_old_remedy(deck),
                    }
                });
            }
            Ok(GatedQuery::Answered(RestartDaemonReply::Accepted { stopping, .. })) => {
                stopped = Some(stopping);
                break;
            }
            Ok(GatedQuery::Answered(RestartDaemonReply::NeedsConfirmation { at_stake, stale })) => {
                // A question whose answer could not be sent is not asked.
                if round + 1 == MAX_CONFIRM_ROUNDS {
                    return not_restarted(NotRestartedReason::StaleConfirmation { at_stake });
                }
                match decider.decide(deck, &at_stake, stale) {
                    RestartChoice::RestartNow => confirm = Some(at_stake),
                    RestartChoice::KeepCurrent => {
                        return not_restarted(NotRestartedReason::KeptByUser { at_stake });
                    }
                    RestartChoice::NoOneToAsk => {
                        return not_restarted(NotRestartedReason::NoOneToAsk { at_stake });
                    }
                }
            }
            Ok(GatedQuery::Answered(RestartDaemonReply::Refused { reason, message })) => {
                return match reason {
                    RestartRefusalReason::InProgress => {
                        not_restarted(NotRestartedReason::AnotherRestartInProgress)
                    }
                    _ => restarting_failed(message),
                };
            }
        }
    }
    let Some(stopped) = stopped else {
        // Unreachable with MAX_CONFIRM_ROUNDS >= 1: the last round returns.
        return not_restarted(NotRestartedReason::StaleConfirmation {
            at_stake: confirm.unwrap_or_default(),
        });
    };

    let mut endpoint_emptied = false;
    if plan.successor == RestartSuccessor::ClientSpawns {
        // With the reply lost this is also the check: a daemon that did not
        // accept keeps its endpoint, so nothing is spawned.
        if let Err(reason) = daemon.spawn_successor() {
            // An accepted restart stopped the old daemon's agents. With the
            // reply lost, whoever answers now says whether it went.
            let gone = reply_lost.is_none() || !old_daemon_answers(daemon, &from);
            return failed_at(
                UpgradeStage::Restarting,
                match &reply_lost {
                    Some(lost) => format!("{lost}, and {reason}"),
                    None => reason,
                },
                gone,
            );
        }
        // `spawn_successor` returns only after the old daemon released the
        // endpoint, so whatever answers now is the successor.
        endpoint_emptied = true;
    }

    progress(UpgradeProgress {
        stage: UpgradeStage::Verifying,
        detail: None,
    });
    let expected = match plan.successor {
        RestartSuccessor::Installed => Expect::Version(installed.version.clone()),
        RestartSuccessor::ClientSpawns => Expect::Build(crate::build_id::local_build_id()),
    };
    match wait_for_successor(daemon, &expected, &from, endpoint_emptied, timing) {
        Ok(()) => UpgradeOutcome::Restarted {
            from_version: from_version.unwrap_or_else(|| "an unknown version".into()),
            to_version: installed.version,
            stopped,
        },
        Err(missing) => match reply_lost {
            // It accepted, so its agents were stopped whether or not it is
            // still the one answering.
            None => failed_at(UpgradeStage::Verifying, missing.reason, true),
            // Nothing says the daemon ever accepted, so this is not a
            // successor that failed to start: it is a restart not known to
            // have happened, and when the old daemon is still the one
            // answering, one that did not.
            Some(lost) if missing.old_still_answering => restarting_failed(format!(
                "{lost}, and the daemon did not restart: the one that was asked is still running after {}s",
                timing.timeout.as_secs()
            )),
            // Not the old daemon answering: gone, or replaced by something
            // that is not the expected successor.
            Some(lost) => failed_at(
                UpgradeStage::Restarting,
                format!(
                    "{lost}, and the daemon did not come back on {} within {}s{}",
                    installed.version,
                    timing.timeout.as_secs(),
                    missing.seen
                ),
                true,
            ),
        },
    }
}

/// Whether the daemon answering `daemon` now is `from`'s own process — the
/// positive identity [`wait_for_successor`] uses. `false` when nothing
/// answers, when what answers is another process, or when `from` named no
/// identity to compare (an older daemon), since it cannot then be shown to be
/// the one still running.
fn old_daemon_answers(daemon: &dyn DaemonPort, from: &OldDaemon) -> bool {
    match daemon.probe() {
        Ok(Some(hello)) => from.instance.is_some() && hello.instance_id == from.instance,
        Ok(None) | Err(_) => false,
    }
}

/// The remedy for a daemon too old to be asked to restart.
fn too_old_remedy(deck: &str) -> String {
    format!(
        "Its agents keep running on the old build. To switch, connect with `dot-agent-deck connect {deck}` and accept its restart prompt, or run `dot-agent-deck daemon restart` on that machine."
    )
}

/// The remedy when the installed build is too old to restart the daemon.
fn installed_too_old_remedy(deck: &str, installed_version: &str) -> String {
    format!(
        "To switch to {installed_version}, run `dot-agent-deck connect {deck}`: the TUI on that machine restarts the daemon onto it, asking first when agents are running."
    )
}

/// What the restarted daemon must report.
enum Expect {
    /// `daemon_version` equal to the installed version.
    Version(String),
    /// `build_version` equal to the client's own build.
    Build(String),
}

/// What the daemon that was asked to restart said about itself, read from its
/// `Hello` before the request.
struct OldDaemon {
    build: Option<String>,
    /// [`AttachResponse::instance_id`]; `None` from a daemon that predates it.
    instance: Option<String>,
}

/// [`wait_for_successor`] ran out: no successor answered.
struct SuccessorMissing {
    /// The whole sentence, for a restart the daemon accepted.
    reason: String,
    /// What last answered, as a parenthetical (empty when nothing did).
    seen: String,
    /// The last answer named the old daemon's own process: it is still running.
    old_still_answering: bool,
}

/// Poll `daemon` until the successor answers as `expected`.
///
/// An answer from the OLD daemon must not count: it keeps answering while it
/// drains, and may go on answering if it never stops. When the old daemon
/// named its process ([`AttachResponse::instance_id`]), a matching answer is
/// the successor's only when it names a different one — the positive identity
/// PRD #1487's review asked for, so the old daemon answering with the same
/// build is never taken for its replacement however long it answers.
///
/// A daemon that predates the field names nothing, and then the older rule
/// applies: a matching answer is the successor's when the endpoint was seen
/// empty in between, or its build differs from the old daemon's, or (a
/// reinstall of the very same build) it has kept matching for `settle` — longer
/// than the old daemon's drain, which is an inference, not a proof.
fn wait_for_successor(
    daemon: &dyn DaemonPort,
    expected: &Expect,
    from: &OldDaemon,
    mut endpoint_emptied: bool,
    timing: Timing,
) -> Result<(), SuccessorMissing> {
    let deadline = Instant::now() + timing.timeout;
    let mut matching_since: Option<Instant> = None;
    let mut last_seen: Option<String> = None;
    // Whether the last answer named the old daemon's process: it never
    // stopped, which the failure then says rather than blaming a successor.
    let mut old_still_answering = false;
    loop {
        // Each probe gets only what is left of the wait, and none starts once
        // less is left than the port can honour: a remote probe can otherwise
        // block well past the deadline (PRD #1487, Qodo 4202262493 and review
        // item 13).
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || remaining < daemon.min_probe_budget() {
            break;
        }
        match daemon.probe_within(remaining) {
            Ok(None) => {
                endpoint_emptied = true;
                matching_since = None;
                old_still_answering = false;
            }
            Ok(Some(hello)) => {
                let matches = match expected {
                    Expect::Version(v) => hello.daemon_version.as_deref() == Some(v.as_str()),
                    Expect::Build(b) => hello.build_version.as_deref() == Some(b.as_str()),
                };
                last_seen = hello.daemon_version.clone().or(hello.build_version.clone());
                old_still_answering = from.instance.is_some() && hello.instance_id == from.instance;
                if matches {
                    if let Some(old) = from.instance.as_deref() {
                        // The identity decides: a different one (or none, from
                        // a successor that predates the field) is another
                        // process; the same one is the old daemon.
                        if hello.instance_id.as_deref() != Some(old) {
                            return Ok(());
                        }
                    } else {
                        let replaced = endpoint_emptied
                            || hello.build_version.as_deref() != from.build.as_deref();
                        if replaced {
                            return Ok(());
                        }
                        let since = *matching_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= timing.settle {
                            return Ok(());
                        }
                    }
                } else {
                    matching_since = None;
                }
            }
            // A probe that learned nothing is neither an answer nor a gap.
            Err(_) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(timing.poll.min(remaining));
    }
    let seen = match (last_seen, old_still_answering) {
        (Some(v), true) => format!(
            " (the daemon answering reports {v}, and it is still the daemon that was asked to restart)"
        ),
        (Some(v), false) => format!(" (the daemon answering reports {v})"),
        (None, _) => String::new(),
    };
    Err(SuccessorMissing {
        reason: format!(
            "restarted, but the new daemon did not answer within {}s{seen}",
            timing.timeout.as_secs()
        ),
        seen,
        old_still_answering,
    })
}

// ---------------------------------------------------------------------------
// When to offer an upgrade (D8)
// ---------------------------------------------------------------------------

/// This client's own version — what an upgrade installs by default (D11).
pub const CLIENT_VERSION: &str = env!("DAD_VERSION");

/// A release version from a daemon or binary report: an optional `v` prefix is
/// dropped, and so is a `-g<sha>[-dirty]` build stamp, which says which commit
/// and not which release.
fn release_version(raw: &str) -> Option<semver::Version> {
    let raw = raw.trim();
    let mut version = semver::Version::parse(raw.strip_prefix('v').unwrap_or(raw)).ok()?;
    let pre = version.pre.as_str();
    let stamp = pre
        .strip_prefix('g')
        .map(|rest| rest.strip_suffix("-dirty").unwrap_or(rest))
        .is_some_and(|sha| !sha.is_empty() && sha.chars().all(|c| c.is_ascii_hexdigit()));
    if stamp {
        version.pre = semver::Prerelease::EMPTY;
    }
    version.build = semver::BuildMetadata::EMPTY;
    Some(version)
}

/// Whether `client` is strictly newer than `daemon`. Never true for a version
/// either side cannot parse, so a malformed report never offers an upgrade.
pub fn client_is_newer(client: &str, daemon: &str) -> bool {
    match (release_version(client), release_version(daemon)) {
        (Some(c), Some(d)) => c > d,
        _ => false,
    }
}

/// Whether a client should offer to upgrade a daemon. Computed in Rust so no
/// client compares versions on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum UpgradeOffer {
    /// The daemon is older: offer to upgrade it from `from` to `to`.
    Offered { from: String, to: String },
    /// Same release (the build stamps may differ).
    Current,
    /// The daemon is newer than this client: nothing to offer.
    DaemonNewer { daemon: String },
    /// The daemon's version is not known or not readable.
    Unknown,
}

/// [`UpgradeOffer`] for a daemon reporting `daemon_version`, against
/// [`CLIENT_VERSION`].
pub fn upgrade_offer(daemon_version: Option<&str>) -> UpgradeOffer {
    upgrade_offer_against(CLIENT_VERSION, daemon_version)
}

/// [`upgrade_offer`] against an explicit client version.
pub fn upgrade_offer_against(client: &str, daemon_version: Option<&str>) -> UpgradeOffer {
    let Some(daemon) = daemon_version else {
        return UpgradeOffer::Unknown;
    };
    match (release_version(client), release_version(daemon)) {
        (Some(c), Some(d)) if c > d => UpgradeOffer::Offered {
            from: daemon.to_string(),
            to: client.to_string(),
        },
        (Some(c), Some(d)) if c == d => UpgradeOffer::Current,
        (Some(_), Some(_)) => UpgradeOffer::DaemonNewer {
            daemon: daemon.to_string(),
        },
        _ => UpgradeOffer::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Production installers
// ---------------------------------------------------------------------------

/// ssh keepalive parameters for the install and restart sessions: detect a
/// dead link within roughly `INTERVAL * COUNT_MAX` seconds (~2 min) without a
/// hard wallclock cap that would kill a slow-but-alive release download.
pub const UPGRADE_SSH_CONNECT_TIMEOUT: u64 = 30;
pub const UPGRADE_SSH_ALIVE_INTERVAL: u64 = 15;
pub const UPGRADE_SSH_ALIVE_COUNT_MAX: u32 = 8;

/// The ssh executor every upgrade session uses.
pub fn upgrade_ssh_executor() -> SystemSshExecutor {
    SystemSshExecutor::with_keepalive(
        UPGRADE_SSH_CONNECT_TIMEOUT,
        UPGRADE_SSH_ALIVE_INTERVAL,
        UPGRADE_SSH_ALIVE_COUNT_MAX,
    )
}

/// Installs on a registered remote through
/// [`crate::remote::upgrade_entry_reporting_to`] — the download to
/// `~/.local/bin`, or `brew upgrade` where Homebrew owns the install —
/// refreshing the hooks and the deck list's row. What that prints goes to
/// `out`.
///
/// It installs on the machine `entry` reaches — the row the caller read once
/// and also built its [`crate::remote_daemon::SshDaemonPort`] from — rather
/// than reading the row again by name, so the install and the restart cannot
/// land on two machines when the row changes mid-upgrade; a row moved
/// elsewhere in the meantime fails the install instead of being written over
/// (PRD #1487, Greptile 4208066970).
pub struct SshInstaller<E: SshExecutor = SystemSshExecutor> {
    pub entry: RemoteEntry,
    pub remotes_path: PathBuf,
    pub executor: E,
    /// `remote upgrade --no-install`: only verify what is already there.
    pub no_install: bool,
    pub release_base: String,
    pub out: RefCell<Box<dyn Write>>,
}

impl SshInstaller<SystemSshExecutor> {
    /// The production installer for the deck-list row `entry`, reporting to
    /// `out`.
    pub fn for_entry(entry: &RemoteEntry, remotes_path: PathBuf, out: Box<dyn Write>) -> Self {
        Self {
            entry: entry.clone(),
            remotes_path,
            executor: upgrade_ssh_executor(),
            no_install: false,
            release_base: crate::remote::RELEASE_BASE.to_string(),
            out: RefCell::new(out),
        }
    }
}

impl<E: SshExecutor> Installer for SshInstaller<E> {
    fn install(&self, version: &str) -> Result<InstalledBuild, InstallError> {
        let opts = crate::remote::UpgradeOptions {
            name: self.entry.name.clone(),
            version: version.to_string(),
            no_install: self.no_install,
            release_base: self.release_base.clone(),
        };
        let mut out = self.out.borrow_mut();
        let entry = crate::remote::upgrade_entry_reporting_to(
            &opts,
            &self.entry,
            &self.executor,
            &self.remotes_path,
            &mut *out,
        )
        .map_err(|e| InstallError {
            installed_version: e.installed_version().map(str::to_string),
            reason: e.to_string(),
        })?;
        let method = if entry.install.as_deref() == Some(crate::remote::INSTALL_HOMEBREW) {
            InstallMethod::Homebrew
        } else {
            InstallMethod::LocalBin
        };
        Ok(InstalledBuild {
            version: entry.version.clone(),
            method,
            binary: entry.remote_binary().to_string(),
        })
    }
}

/// The local Replace: nothing to install, the client's own build is the new
/// one.
pub struct NoInstall;

impl Installer for NoInstall {
    fn install(&self, _version: &str) -> Result<InstalledBuild, InstallError> {
        let binary = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Ok(InstalledBuild {
            version: CLIENT_VERSION.to_string(),
            method: InstallMethod::ClientBuild,
            binary,
        })
    }
}

// ---------------------------------------------------------------------------
// Production daemon ports
// ---------------------------------------------------------------------------

/// What a remote `daemon probe --json` says, as the port reports it.
///
/// `running` and `hello` must agree: a report that a daemon is running with no
/// `Hello` from it (or a `Hello` from a daemon it says is not running) cannot
/// be read as "no daemon running", which would skip the restart silently, so
/// it is a failed probe (Qodo 4202911031).
fn remote_probe_answer<E: SshExecutor>(
    port: &SshDaemonPort<E>,
    probe: Result<crate::daemon_restart::DaemonProbe, RemoteDaemonError>,
) -> Result<Option<AttachResponse>, PortError> {
    match probe {
        Ok(probe) => match (probe.running, probe.hello) {
            (true, Some(hello)) => Ok(Some(hello)),
            (false, None) => Ok(None),
            (true, None) => Err(PortError::Other(format!(
                "the installed build at {} reported a running daemon but not its answer",
                port.binary()
            ))),
            (false, Some(_)) => Err(PortError::Other(format!(
                "the installed build at {} reported no running daemon but included its answer",
                port.binary()
            ))),
        },
        Err(RemoteDaemonError::Unsupported { .. }) => {
            Err(PortError::InstalledBuildTooOld(format!(
                "the installed build at {} is too old to report on the running daemon",
                port.binary()
            )))
        }
        Err(e) => Err(PortError::Other(e.to_string())),
    }
}

/// A remote daemon, reached through the remote's freshly installed binary.
impl<E: SshExecutor> DaemonPort for SshDaemonPort<E> {
    fn probe(&self) -> Result<Option<AttachResponse>, PortError> {
        remote_probe_answer(self, SshDaemonPort::probe(self))
    }

    fn probe_within(&self, budget: Duration) -> Result<Option<AttachResponse>, PortError> {
        remote_probe_answer(self, SshDaemonPort::probe_within(self, budget))
    }

    fn min_probe_budget(&self) -> Duration {
        SshDaemonPort::min_probe_budget(self)
    }

    fn restart(
        &self,
        req: &RestartDaemonRequest,
    ) -> Result<GatedQuery<RestartDaemonReply>, PortError> {
        match self.restart_installed(req.expected_version.as_deref(), req.confirm.as_ref()) {
            Ok(report) if report.unsupported => Ok(GatedQuery::Unsupported),
            Ok(report) => match report.reply {
                Some(reply) => Ok(GatedQuery::Answered(reply)),
                // The daemon went away between the probe and the request: the
                // build is installed and the next start runs it (Qodo
                // 4202060284).
                None if !report.running => Err(PortError::NoDaemonRunning),
                None => Err("the remote reported no answer from the daemon".into()),
            },
            // The installed build is too old to drive the restart (Homebrew
            // can land an older tap release): the daemon was never asked, and
            // it is the installed build that is too old, not the daemon.
            Err(RemoteDaemonError::Unsupported { .. }) => {
                Err(PortError::InstalledBuildTooOld(format!(
                    "the installed build at {} is too old to ask the running daemon to restart",
                    self.binary()
                )))
            }
            // The same build just answered `daemon probe`, so it is new enough;
            // the reply went bad after the request ran, and the remote may
            // already have restarted. `reason` says whether output arrived and
            // failed to parse or none arrived (Qodo 4218118658).
            Err(RemoteDaemonError::Malformed(reason)) => Err(PortError::ReplyUnreadable(format!(
                "the remote's reply to the restart request was not usable: {reason}"
            ))),
            // Failures from before the request was sent: ssh never got a
            // session, or the remote binary said it did not send it.
            Err(
                e @ RemoteDaemonError::Ssh(
                    SshError::ConnectionRefused { .. }
                    | SshError::AuthFailed { .. }
                    | SshError::HostKeyVerificationFailed { .. },
                ),
            ) => Err(PortError::Other(e.to_string())),
            Err(e @ RemoteDaemonError::Failed { status, .. })
                if status == i32::from(crate::daemon_restart::RESTART_NOT_SENT_EXIT) =>
            {
                Err(PortError::Other(e.to_string()))
            }
            // Anything else can happen after the request reached the daemon —
            // the session dropping, the deadline kill, the remote binary
            // timing out or losing the reply — so the daemon may be restarting.
            Err(e) => Err(PortError::ReplyUnreadable(format!(
                "no answer to the restart request was read: {e}"
            ))),
        }
    }

    fn installed(&self, build: &InstalledBuild) {
        // Where the upgrade's raw string reaches the port's binary (issue
        // #1490): validated here, so a value that is neither the default install nor a
        // safe absolute path never reaches a remote shell. The remote installer
        // records a validated row's binary, so a refusal means a bug, and the
        // port keeps running the binary it already had.
        match crate::remote::RemoteDeckBinary::try_from(build.binary.as_str()) {
            Ok(binary) => self.set_binary(binary),
            Err(error) => tracing::warn!(
                target: "remote",
                %error,
                "not repointing the remote daemon port at an unsafe binary path"
            ),
        }
    }
}

/// How long the local Replace waits for the old daemon to release its endpoint
/// before starting the client's build.
const LOCAL_RELEASE_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on one local probe.
const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A daemon on this machine, over its socket — the local Replace. `spawn`
/// starts the client's own build (the desktop's `bootstrap(start_if_missing)`).
///
/// Every method blocks on `handle`, so it must be called from a blocking
/// thread (`spawn_blocking`), never from inside the runtime.
pub struct WireDaemonPort {
    client: DaemonClient,
    handle: tokio::runtime::Handle,
    local: Option<LocalEndpoint>,
    spawn: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
}

impl WireDaemonPort {
    pub fn new(
        endpoint: &Endpoint,
        handle: tokio::runtime::Handle,
        spawn: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Result<Self, String> {
        Ok(Self {
            client: DaemonClient::for_endpoint(endpoint).map_err(|e| e.to_string())?,
            handle,
            local: endpoint.as_local().cloned(),
            spawn,
        })
    }

    /// Wait for the old daemon to release the endpoint, then start the
    /// client's build.
    ///
    /// Only `Ok(None)` means released. A probe error is inconclusive, not a
    /// verdict: a daemon shutting down can accept the connection and close it
    /// before its `Hello` reply, so polling continues to the deadline
    /// (Qodo 4219656676).
    fn release_then_spawn(&self) -> Result<(), String> {
        let deadline = Instant::now() + LOCAL_RELEASE_TIMEOUT;
        loop {
            let last_error = match DaemonPort::probe(self) {
                Ok(None) => return (self.spawn)(),
                Ok(Some(_)) => None,
                Err(e) => Some(e.to_string()),
            };
            if Instant::now() >= deadline {
                let secs = LOCAL_RELEASE_TIMEOUT.as_secs();
                return Err(match last_error {
                    Some(e) => format!(
                        "the daemon that was asked to restart had not released its endpoint \
                         {secs}s later; the last check failed: {e}"
                    ),
                    None => format!(
                        "the daemon that was asked to restart was still running {secs}s later"
                    ),
                });
            }
            std::thread::sleep(VERIFY_POLL);
        }
    }
}

impl DaemonPort for WireDaemonPort {
    fn probe(&self) -> Result<Option<AttachResponse>, PortError> {
        DaemonPort::probe_within(self, LOCAL_PROBE_TIMEOUT)
    }

    fn probe_within(&self, budget: Duration) -> Result<Option<AttachResponse>, PortError> {
        let bound = LOCAL_PROBE_TIMEOUT.min(budget);
        self.handle
            .block_on(async { tokio::time::timeout(bound, self.client.probe_running()).await })
            .map_err(|_| {
                format!(
                    "no answer from the daemon within {:.1}s",
                    bound.as_secs_f64()
                )
            })?
            .map_err(|e| PortError::Other(e.to_string()))
    }

    fn restart(
        &self,
        req: &RestartDaemonRequest,
    ) -> Result<GatedQuery<RestartDaemonReply>, PortError> {
        self.handle
            .block_on(self.client.restart_daemon(req.clone()))
            .map_err(|e| match e {
                // Sent, then no usable answer: the daemon may be restarting.
                ClientError::Unanswered(_) | ClientError::Malformed(_) => {
                    PortError::ReplyUnreadable(e.to_string())
                }
                // Nothing listening, before anything was sent: the daemon
                // exited after the first probe, as the SSH port reports it.
                ClientError::Io(ref io)
                    if matches!(
                        io.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    PortError::NoDaemonRunning
                }
                e => PortError::Other(e.to_string()),
            })
    }

    fn spawn_successor(&self) -> Result<(), String> {
        self.release_then_spawn()
    }

    /// A local daemon without the restart request: the existing stop path,
    /// for an idle daemon only. Only for a local deck; a remote one gets
    /// `None`.
    fn legacy_restart(&self) -> Option<UpgradeOutcome> {
        let local = self.local.clone()?;
        Some(self.legacy_restart_local(&local))
    }
}

impl WireDaemonPort {
    /// Replace a local daemon that predates the restart request — only when
    /// nothing runs on it (PRD #1487 review).
    ///
    /// Such a daemon cannot be asked to hold new agent starts while someone
    /// answers "these would stop; restart now?", so an answer could never
    /// cover what the stop would actually take down: an agent started while
    /// the question was open would be stopped unnamed. So no one is asked.
    /// The unforced [`run_daemon_stop`](crate::daemon_stop::run_daemon_stop)
    /// lists the daemon's agents and orchestration roles itself immediately
    /// before it signals, and refuses if there are any; that refusal becomes
    /// [`NotRestartedReason::OlderDaemonBusy`], naming them.
    ///
    /// **Residual window:** between that listing and the signal (one local
    /// round trip, then the signal) an agent can still be started by another
    /// client or a schedule, and it is stopped with the daemon. An older
    /// daemon has no way to freeze new starts, so this narrows the window
    /// from "while the user decides" to that gap; it does not close it.
    fn legacy_restart_local(&self, local: &LocalEndpoint) -> UpgradeOutcome {
        use crate::daemon_stop::{StopError, StopOutcome, run_daemon_stop};
        let version = CLIENT_VERSION.to_string();
        let failed = |stage, reason: String, old_daemon_gone| UpgradeOutcome::Failed {
            stage,
            reason,
            installed_version: Some(version.clone()),
            old_daemon_gone,
        };
        let hello = match DaemonPort::probe(self) {
            Ok(Some(hello)) => hello,
            Ok(None) => {
                return UpgradeOutcome::InstalledNotRestarted {
                    from_version: None,
                    installed_version: version,
                    reason: NotRestartedReason::NoDaemonRunning,
                };
            }
            Err(reason) => return failed(UpgradeStage::Restarting, reason.to_string(), false),
        };
        let from_version = hello.daemon_version.clone();
        let from = OldDaemon {
            build: None,
            instance: hello.instance_id.clone(),
        };
        match self.handle.block_on(run_daemon_stop(local, false)) {
            Ok(StopOutcome::NoDaemonRunning | StopOutcome::Stopped { .. })
            | Ok(StopOutcome::ForceKilled { .. }) => {}
            Err(
                refusal @ (StopError::LiveAgents { .. } | StopError::LiveOrchestrations { .. }),
            ) => {
                let roles = match refusal {
                    StopError::LiveOrchestrations { roles } => roles,
                    _ => Vec::new(),
                };
                // Naming the agents is best-effort: the stop already refused,
                // so a failed listing changes only what the outcome names.
                let agents = self
                    .handle
                    .block_on(self.client.list_agents())
                    .unwrap_or_default();
                return UpgradeOutcome::InstalledNotRestarted {
                    from_version,
                    installed_version: version,
                    reason: NotRestartedReason::OlderDaemonBusy {
                        at_stake: crate::daemon_restart::stop_set(&roles, &agents),
                    },
                };
            }
            Err(e) => return failed(UpgradeStage::Restarting, e.to_string(), false),
        }
        // From here the old daemon was stopped.
        if let Err(reason) = self.release_then_spawn() {
            return failed(UpgradeStage::Restarting, reason, true);
        }
        let expected = Expect::Build(crate::build_id::local_build_id());
        match wait_for_successor(self, &expected, &from, true, PRODUCTION_TIMING) {
            Ok(()) => UpgradeOutcome::Restarted {
                from_version: from_version.unwrap_or_else(|| "an unknown version".into()),
                to_version: version,
                stopped: RestartStopSet::default(),
            },
            Err(missing) => failed(UpgradeStage::Verifying, missing.reason, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::{PROTOCOL_VERSION, RestartAgent};
    use crate::state::OrchestrationRoleRecord;
    use std::cell::Cell;
    use std::collections::VecDeque;

    const FAST: Timing = Timing {
        timeout: Duration::from_millis(200),
        poll: Duration::from_millis(1),
        settle: Duration::from_millis(50),
    };

    struct FakeInstaller {
        result: Result<InstalledBuild, InstallError>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeInstaller {
        fn ok(version: &str, method: InstallMethod) -> Self {
            Self {
                result: Ok(InstalledBuild {
                    version: version.into(),
                    method,
                    binary: match method {
                        InstallMethod::Homebrew => "/opt/homebrew/bin/dot-agent-deck".into(),
                        _ => "~/.local/bin/dot-agent-deck".into(),
                    },
                }),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn failing(reason: &str) -> Self {
            Self {
                result: Err(reason.to_string().into()),
                calls: RefCell::new(Vec::new()),
            }
        }
        /// The build landed, then a later install step failed.
        fn partly(version: &str, reason: &str) -> Self {
            Self {
                result: Err(InstallError {
                    reason: reason.into(),
                    installed_version: Some(version.into()),
                }),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Installer for FakeInstaller {
        fn install(&self, version: &str) -> Result<InstalledBuild, InstallError> {
            self.calls.borrow_mut().push(version.into());
            self.result.clone()
        }
    }

    fn hello(version: &str, build: &str) -> AttachResponse {
        let mut h = AttachResponse::hello(PROTOCOL_VERSION);
        h.daemon_version = Some(version.into());
        h.build_version = Some(build.into());
        h
    }

    type Probe = Result<Option<AttachResponse>, PortError>;
    type Restart = Result<GatedQuery<RestartDaemonReply>, PortError>;

    /// Scripted probes and restart replies; the last probe repeats.
    struct FakePort {
        probes: RefCell<VecDeque<Probe>>,
        restarts: RefCell<VecDeque<Restart>>,
        requests: RefCell<Vec<RestartDaemonRequest>>,
        installed: RefCell<Option<InstalledBuild>>,
        spawned: Cell<usize>,
        spawn_result: Result<(), String>,
        legacy: Option<UpgradeOutcome>,
    }

    impl FakePort {
        fn new(probes: Vec<Probe>, restarts: Vec<Restart>) -> Self {
            Self {
                probes: RefCell::new(probes.into()),
                restarts: RefCell::new(restarts.into()),
                requests: RefCell::new(Vec::new()),
                installed: RefCell::new(None),
                spawned: Cell::new(0),
                spawn_result: Ok(()),
                legacy: None,
            }
        }
    }

    impl DaemonPort for FakePort {
        fn probe(&self) -> Probe {
            let mut probes = self.probes.borrow_mut();
            if probes.len() > 1 {
                probes.pop_front().unwrap()
            } else {
                probes.front().cloned().unwrap_or(Ok(None))
            }
        }
        fn restart(&self, req: &RestartDaemonRequest) -> Restart {
            self.requests.borrow_mut().push(req.clone());
            self.restarts
                .borrow_mut()
                .pop_front()
                .expect("an unscripted restart request")
        }
        fn installed(&self, build: &InstalledBuild) {
            *self.installed.borrow_mut() = Some(build.clone());
        }
        fn spawn_successor(&self) -> Result<(), String> {
            self.spawned.set(self.spawned.get() + 1);
            self.spawn_result.clone()
        }
        fn legacy_restart(&self) -> Option<UpgradeOutcome> {
            self.legacy.clone()
        }
    }

    /// Answers from a script and records each question.
    struct Scripted {
        answers: RefCell<VecDeque<RestartChoice>>,
        asked: RefCell<Vec<(RestartStopSet, bool)>>,
    }

    impl Scripted {
        fn new(answers: &[RestartChoice]) -> Self {
            Self {
                answers: RefCell::new(answers.iter().copied().collect()),
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl RestartDecider for Scripted {
        fn decide(&self, _deck: &str, at_stake: &RestartStopSet, stale: bool) -> RestartChoice {
            self.asked.borrow_mut().push((at_stake.clone(), stale));
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("an unscripted question")
        }
    }

    fn live(agent: &str) -> RestartStopSet {
        RestartStopSet {
            agents: vec![RestartAgent {
                id: format!("id-{agent}"),
                label: agent.into(),
                pane_id: Some(format!("pane-{agent}")),
                cwd: Some("/work".into()),
            }],
            roles: vec![OrchestrationRoleRecord {
                pane_id: format!("pane-{agent}"),
                role: "lead".into(),
                orchestration: "team".into(),
                is_orchestrator: true,
            }],
        }
    }

    fn accepted(stopping: RestartStopSet) -> Restart {
        Ok(GatedQuery::Answered(RestartDaemonReply::Accepted {
            from_version: "0.1.0".into(),
            to_version: Some("0.2.0".into()),
            successor: RestartSuccessor::Installed,
            stopping,
        }))
    }

    fn needs(at_stake: RestartStopSet, stale: bool) -> Restart {
        Ok(GatedQuery::Answered(
            RestartDaemonReply::NeedsConfirmation { at_stake, stale },
        ))
    }

    fn remote_plan() -> UpgradePlan {
        UpgradePlan {
            version: "0.2.0".into(),
            successor: RestartSuccessor::Installed,
        }
    }

    fn run(
        installer: &dyn Installer,
        port: &dyn DaemonPort,
        decider: &dyn RestartDecider,
        plan: &UpgradePlan,
    ) -> (UpgradeOutcome, Vec<UpgradeStage>) {
        let mut stages = Vec::new();
        let outcome = run_upgrade(
            "box",
            plan,
            installer,
            port,
            decider,
            &mut |p| stages.push(p.stage),
            FAST,
        );
        (outcome, stages)
    }

    #[test]
    fn idle_daemon_restarts_without_asking_even_with_no_decider() {
        for method in [InstallMethod::LocalBin, InstallMethod::Homebrew] {
            let installer = FakeInstaller::ok("0.2.0", method);
            let port = FakePort::new(
                vec![
                    Ok(Some(hello("0.1.0", "0.1.0-gold"))),
                    Ok(Some(hello("0.1.0", "0.1.0-gold"))),
                    Ok(None),
                    Ok(Some(hello("0.2.0", "0.2.0-gnew"))),
                ],
                vec![accepted(RestartStopSet::default())],
            );
            let (outcome, stages) = run(&installer, &port, &NoDecider, &remote_plan());
            assert_eq!(
                outcome,
                UpgradeOutcome::Restarted {
                    from_version: "0.1.0".into(),
                    to_version: "0.2.0".into(),
                    stopped: RestartStopSet::default(),
                }
            );
            assert!(!outcome.is_failure());
            assert_eq!(
                stages,
                [
                    UpgradeStage::Installing,
                    UpgradeStage::Restarting,
                    UpgradeStage::Verifying
                ]
            );
            assert_eq!(installer.calls.borrow().as_slice(), ["0.2.0"]);
            // The port runs what the installer put in place.
            assert_eq!(port.installed.borrow().as_ref().unwrap().method, method);
            let requests = port.requests.borrow();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].confirm, None);
            assert_eq!(requests[0].expected_version.as_deref(), Some("0.2.0"));
            assert_eq!(requests[0].successor, RestartSuccessor::Installed);
        }
    }

    #[test]
    fn install_failure_touches_no_daemon() {
        let port = FakePort::new(vec![], vec![]);
        let (outcome, stages) = run(
            &FakeInstaller::failing("download refused"),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::Failed {
                stage: UpgradeStage::Installing,
                reason: "download refused".into(),
                installed_version: None,
                old_daemon_gone: false,
            }
        );
        assert!(outcome.is_failure());
        assert_eq!(stages, [UpgradeStage::Installing]);
        assert!(port.requests.borrow().is_empty());
        assert!(port.installed.borrow().is_none());
    }

    /// PRD #1487 review: the build landed and a later install step (hooks,
    /// deck list) failed. The failure keeps the installed version and the
    /// step, touches no daemon, and its summary says the new build is
    /// installed and how to finish — never that nothing was changed.
    #[test]
    fn a_partial_install_keeps_its_version_and_says_how_to_finish() {
        let port = FakePort::new(vec![], vec![]);
        let reason =
            "0.2.0 was installed, but reinstalling the hooks failed: settings.json is not writable";
        let (outcome, stages) = run(
            &FakeInstaller::partly("0.2.0", reason),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::Failed {
                stage: UpgradeStage::Installing,
                reason: reason.into(),
                installed_version: Some("0.2.0".into()),
                old_daemon_gone: false,
            }
        );
        assert_eq!(stages, [UpgradeStage::Installing]);
        assert!(port.requests.borrow().is_empty());
        let summary = outcome.summary("box");
        assert!(
            summary.contains("reinstalling the hooks failed"),
            "{summary}"
        );
        assert!(
            summary.contains(
                "0.2.0 is installed, but the upgrade stopped before restarting the daemon"
            ),
            "{summary}"
        );
        assert!(
            summary.contains("dot-agent-deck remote upgrade box"),
            "{summary}"
        );
        assert!(
            !summary.to_lowercase().contains("nothing was changed"),
            "{summary}"
        );

        // The binary was replaced but its version check read nothing (PRD
        // #1487 review, Qodo 4200041523): it is named as an unverified build,
        // opening its sentence, not as nothing having changed.
        let unverified = UpgradeOutcome::Failed {
            stage: UpgradeStage::Installing,
            reason: "~/.local/bin/dot-agent-deck on the remote was replaced, but the new binary did not pass its version check".into(),
            installed_version: Some(crate::remote::UNVERIFIED_BUILD.into()),
            old_daemon_gone: false,
        }
        .summary("box");
        assert!(
            unverified.contains(
                "\nAn unverified build is installed, but the upgrade stopped before restarting the daemon"
            ),
            "{unverified}"
        );
        assert!(
            unverified.contains("dot-agent-deck remote upgrade box"),
            "{unverified}"
        );

        // A failure before anything landed does not claim a version either.
        let failed = UpgradeOutcome::Failed {
            stage: UpgradeStage::Installing,
            reason: "download refused".into(),
            installed_version: None,
            old_daemon_gone: false,
        }
        .summary("box");
        assert!(!failed.contains("is installed"), "{failed}");
        assert!(
            !failed.to_lowercase().contains("nothing was changed"),
            "{failed}"
        );
    }

    /// Scenario: a restart fails after its reply was lost and the old daemon
    /// no longer answers as itself. The summary does not claim the old daemon
    /// keeps running: it says it may have stopped and that connecting shows
    /// what answers now. A restart the old daemon refused still says it keeps
    /// running (PRD #1487, Qodo 4201244671).
    #[test]
    fn a_restart_failure_with_the_old_daemon_gone_does_not_say_it_keeps_running() {
        for installed_version in [Some("0.45.0".to_string()), None] {
            let gone = UpgradeOutcome::Failed {
                stage: UpgradeStage::Restarting,
                reason: "the restart reply was lost".into(),
                installed_version: installed_version.clone(),
                old_daemon_gone: true,
            }
            .summary("box");
            assert!(!gone.contains("keeps running"), "{gone}");
            assert!(
                gone.contains("The daemon that was running may have stopped"),
                "{gone}"
            );
            assert!(
                gone.contains("`dot-agent-deck connect box` shows what is answering now"),
                "{gone}"
            );
            assert_eq!(
                gone.contains("0.45.0 is installed"),
                installed_version.is_some(),
                "{gone}"
            );

            let refused = UpgradeOutcome::Failed {
                stage: UpgradeStage::Restarting,
                reason: "refused".into(),
                installed_version,
                old_daemon_gone: false,
            }
            .summary("box");
            assert!(
                refused.contains("the daemon that was running keeps running")
                    || refused.contains("The daemon that was running keeps running"),
                "{refused}"
            );
        }
    }

    #[test]
    fn no_daemon_running_installs_and_restarts_nothing() {
        let port = FakePort::new(vec![Ok(None)], vec![]);
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: "0.2.0".into(),
                reason: NotRestartedReason::NoDaemonRunning,
            }
        );
        assert!(port.requests.borrow().is_empty());
    }

    #[test]
    fn live_work_with_no_one_to_ask_installs_and_keeps() {
        let port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "b")))],
            vec![needs(live("a"), false)],
        );
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::InstalledNotRestarted {
                from_version: Some("0.1.0".into()),
                installed_version: "0.2.0".into(),
                reason: NotRestartedReason::NoOneToAsk {
                    at_stake: live("a")
                },
            }
        );
        assert!(!outcome.is_failure());
    }

    #[test]
    fn keep_current_is_kept_by_user() {
        let port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "b")))],
            vec![needs(live("a"), false)],
        );
        let decider = Scripted::new(&[RestartChoice::KeepCurrent]);
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::Homebrew),
            &port,
            &decider,
            &remote_plan(),
        );
        assert!(matches!(
            outcome,
            UpgradeOutcome::InstalledNotRestarted {
                reason: NotRestartedReason::KeptByUser { .. },
                ..
            }
        ));
        assert_eq!(port.requests.borrow().len(), 1, "keep sends nothing more");
    }

    #[test]
    fn restart_now_confirms_the_named_set() {
        let port = FakePort::new(
            vec![
                Ok(Some(hello("0.1.0", "old"))),
                Ok(Some(hello("0.2.0", "new"))),
            ],
            vec![needs(live("a"), false), accepted(live("a"))],
        );
        let decider = Scripted::new(&[RestartChoice::RestartNow]);
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &decider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::Restarted {
                from_version: "0.1.0".into(),
                to_version: "0.2.0".into(),
                stopped: live("a"),
            }
        );
        assert_eq!(*decider.asked.borrow(), vec![(live("a"), false)]);
        assert_eq!(port.requests.borrow()[1].confirm, Some(live("a")));
    }

    #[test]
    fn a_stop_set_that_keeps_changing_is_stale_confirmation() {
        let port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "old")))],
            vec![
                needs(live("a"), false),
                needs(live("b"), true),
                needs(live("c"), true),
            ],
        );
        let decider = Scripted::new(&[RestartChoice::RestartNow, RestartChoice::RestartNow]);
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &decider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::InstalledNotRestarted {
                from_version: Some("0.1.0".into()),
                installed_version: "0.2.0".into(),
                reason: NotRestartedReason::StaleConfirmation {
                    at_stake: live("c")
                },
            }
        );
        assert_eq!(port.requests.borrow().len(), MAX_CONFIRM_ROUNDS);
        // Asked twice (the second time told it changed); the third set is not
        // asked about, because no fourth request would carry the answer.
        assert_eq!(
            *decider.asked.borrow(),
            vec![(live("a"), false), (live("b"), true)]
        );
        // Each confirmation carries the set the user was shown.
        assert_eq!(port.requests.borrow()[1].confirm, Some(live("a")));
        assert_eq!(port.requests.borrow()[2].confirm, Some(live("b")));
    }

    #[test]
    fn a_stale_set_confirmed_again_restarts() {
        let port = FakePort::new(
            vec![
                Ok(Some(hello("0.1.0", "old"))),
                Ok(Some(hello("0.2.0", "new"))),
            ],
            vec![
                needs(live("a"), false),
                needs(live("b"), true),
                accepted(live("b")),
            ],
        );
        let decider = Scripted::new(&[RestartChoice::RestartNow, RestartChoice::RestartNow]);
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &decider,
            &remote_plan(),
        );
        assert!(matches!(outcome, UpgradeOutcome::Restarted { .. }));
    }

    #[test]
    fn refusals_map_to_in_progress_or_a_restarting_failure() {
        let refused = |reason| {
            Ok(GatedQuery::Answered(RestartDaemonReply::Refused {
                reason,
                message: format!("refused: {reason:?}"),
            }))
        };
        let port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "old")))],
            vec![refused(RestartRefusalReason::InProgress)],
        );
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert!(matches!(
            outcome,
            UpgradeOutcome::InstalledNotRestarted {
                reason: NotRestartedReason::AnotherRestartInProgress,
                ..
            }
        ));
        for reason in [
            RestartRefusalReason::TargetUnresolvable,
            RestartRefusalReason::TargetMissing,
            RestartRefusalReason::TargetDidNotAnswer,
            RestartRefusalReason::VersionMismatch,
            RestartRefusalReason::Unknown,
        ] {
            let port = FakePort::new(vec![Ok(Some(hello("0.1.0", "old")))], vec![refused(reason)]);
            let (outcome, _) = run(
                &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
                &port,
                &NoDecider,
                &remote_plan(),
            );
            assert_eq!(
                outcome,
                UpgradeOutcome::Failed {
                    stage: UpgradeStage::Restarting,
                    reason: format!("refused: {reason:?}"),
                    installed_version: Some("0.2.0".into()),
                    old_daemon_gone: false,
                }
            );
        }
    }

    #[test]
    fn a_daemon_without_the_request_is_too_old_unless_the_port_falls_back() {
        let port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "old")))],
            vec![Ok(GatedQuery::Unsupported)],
        );
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        let UpgradeOutcome::InstalledDaemonTooOld {
            installed_version,
            daemon_version,
            remedy,
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(installed_version, "0.2.0");
        assert_eq!(daemon_version.as_deref(), Some("0.1.0"));
        assert!(remedy.contains("dot-agent-deck connect box"));
        assert!(outcome.summary("box").contains("too old to restart itself"));
        assert!(!outcome.is_failure());

        let mut port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "old")))],
            vec![Ok(GatedQuery::Unsupported)],
        );
        let fallback = UpgradeOutcome::Restarted {
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            stopped: RestartStopSet::default(),
        };
        port.legacy = Some(fallback.clone());
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::ClientBuild),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(outcome, fallback);
    }

    /// PRD #1487 review: Replace against a local daemon too old for the
    /// restart request, with an agent and an orchestration role on it. No one
    /// is asked — an older daemon cannot hold new starts while a question is
    /// open, so an answer could not cover what a stop would take down — and
    /// the daemon is not stopped: the outcome names what runs on it, and no
    /// successor is started. (An idle older daemon is stopped by the same
    /// unforced stop `daemon stop` runs; that half is not exercised here,
    /// because an in-process fake daemon's peer is this test process.)
    #[cfg(unix)]
    #[test]
    fn replace_leaves_a_busy_older_local_daemon_running_and_asks_no_one() {
        use crate::daemon_client::LocalEndpoint;
        use crate::daemon_protocol::{KIND_REQ, read_frame, write_resp};
        use std::os::unix::fs::PermissionsExt;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = crate::test_temp::tempdir().unwrap();
        // Another test's bind can flip the umask while the tempdir is made.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("older.sock");
        let listener = {
            let _guard = rt.enter();
            crate::daemon_protocol::bind_attach_listener(&path).expect("bind the older daemon")
        };
        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let server_ops = ops.clone();
        rt.spawn(async move {
            while let Ok(mut stream) = listener.accept().await {
                let server_ops = server_ops.clone();
                tokio::spawn(async move {
                    while let Ok(Some((KIND_REQ, payload))) = read_frame(&mut stream).await {
                        let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                        let op = request["op"].as_str().unwrap_or_default().to_string();
                        server_ops.lock().unwrap().push(op.clone());
                        let response = if op == "hello" {
                            // An older daemon: no `restart-daemon` advertised.
                            let mut h = hello("0.30.0", "older-build");
                            h.capabilities = Some(Vec::new());
                            h
                        } else {
                            let mut r = AttachResponse::ok();
                            r.agent_records = Some(vec![crate::agent_pty::AgentRecord {
                                id: "a1".into(),
                                pane_id_env: Some("7".into()),
                                display_name: Some("coder".into()),
                                cwd: Some("/work".into()),
                                tab_membership: None,
                                agent_type: None,
                                rows: 24,
                                cols: 80,
                                live: None,
                                spawned_at_ms: None,
                                cli_name: None,
                                prompt_keys: None,
                                crashed: None,
                                orchestrator_context_path: None,
                            }]);
                            r.orchestration_roles = Some(vec![OrchestrationRoleRecord {
                                pane_id: "7".into(),
                                role: "coder".into(),
                                orchestration: "team".into(),
                                is_orchestrator: false,
                            }]);
                            r
                        };
                        if write_resp(&mut stream, &response).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let spawned = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_count = spawned.clone();
        let port = WireDaemonPort::new(
            &Endpoint::Local(LocalEndpoint::at(path)),
            rt.handle().clone(),
            Box::new(move || {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
        // No answers scripted: asking anything panics the test.
        let decider = Scripted::new(&[]);
        let plan = UpgradePlan {
            version: CLIENT_VERSION.into(),
            successor: RestartSuccessor::ClientSpawns,
        };
        let (outcome, _) = run(&NoInstall, &port, &decider, &plan);

        let UpgradeOutcome::InstalledNotRestarted {
            from_version,
            reason: NotRestartedReason::OlderDaemonBusy { at_stake },
            ..
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(from_version.as_deref(), Some("0.30.0"));
        assert_eq!(at_stake.agents.len(), 1);
        assert_eq!(at_stake.agents[0].label, "coder");
        assert_eq!(at_stake.roles.len(), 1);
        assert!(decider.asked.borrow().is_empty(), "no one is asked");
        assert_eq!(spawned.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!outcome.is_failure());
        let summary = outcome.summary("local");
        assert!(
            summary.contains("Stop them, or let them finish"),
            "{summary}"
        );
        assert!(summary.contains("coder"), "{summary}");
        assert!(
            !ops.lock().unwrap().iter().any(|op| op.contains("restart")),
            "a daemon that does not advertise the request is never sent it"
        );
        drop(port);
        rt.shutdown_background();
    }

    /// PRD #1487 review (Qodo 4200422524), the local port: a daemon that
    /// reads the restart request and closes the connection without answering
    /// may be restarting, so the port reports an unread reply for the upgrade
    /// to verify, not a failure.
    #[cfg(unix)]
    #[test]
    fn the_local_port_reports_a_reply_lost_after_the_send_as_unread() {
        use crate::daemon_client::LocalEndpoint;
        use crate::daemon_protocol::{CAP_RESTART_DAEMON, KIND_REQ, read_frame, write_resp};
        use std::os::unix::fs::PermissionsExt;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = crate::test_temp::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("lossy.sock");
        let listener = {
            let _guard = rt.enter();
            crate::daemon_protocol::bind_attach_listener(&path).expect("bind the daemon")
        };
        rt.spawn(async move {
            while let Ok(mut stream) = listener.accept().await {
                let Ok(Some((KIND_REQ, payload))) = read_frame(&mut stream).await else {
                    continue;
                };
                let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                if request["op"] == "hello" {
                    let mut h = hello("0.30.0", "old");
                    h.capabilities = Some(vec![CAP_RESTART_DAEMON.to_string()]);
                    let _ = write_resp(&mut stream, &h).await;
                }
                // The restart request: dropped unanswered.
            }
        });
        let port = WireDaemonPort::new(
            &Endpoint::Local(LocalEndpoint::at(path)),
            rt.handle().clone(),
            Box::new(|| Ok(())),
        )
        .unwrap();
        let restarted = DaemonPort::restart(&port, &RestartDaemonRequest::default());
        assert!(
            matches!(&restarted, Err(PortError::ReplyUnreadable(_))),
            "{restarted:?}"
        );
        drop(port);
        rt.shutdown_background();
    }

    /// Scenario: the old daemon is shutting down after accepting a restart. It
    /// accepts one more probe connection and closes it without a `Hello`
    /// reply, then releases its endpoint. The local Replace keeps polling
    /// through the failed probe and starts the successor, instead of giving up
    /// with no daemon running (Qodo 4219656676).
    #[cfg(unix)]
    #[test]
    fn the_local_port_spawns_after_a_probe_lost_during_shutdown() {
        use crate::daemon_client::LocalEndpoint;
        use std::os::unix::fs::PermissionsExt;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = crate::test_temp::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("closing.sock");
        let listener = {
            let _guard = rt.enter();
            crate::daemon_protocol::bind_attach_listener(&path).expect("bind the daemon")
        };
        let socket = path.clone();
        rt.spawn(async move {
            // One probe connects and loses its reply; then the endpoint goes.
            if let Ok(stream) = listener.accept().await {
                drop(stream);
            }
            let _ = std::fs::remove_file(&socket);
            drop(listener);
        });

        let spawned = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_count = spawned.clone();
        let port = WireDaemonPort::new(
            &Endpoint::Local(LocalEndpoint::at(path)),
            rt.handle().clone(),
            Box::new(move || {
                spawn_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
        assert_eq!(DaemonPort::spawn_successor(&port), Ok(()));
        assert_eq!(spawned.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(port);
        rt.shutdown_background();
    }

    /// PRD #1487 review (Qodo summary #16), the local port: a daemon that
    /// exited between the first probe and the restart leaves nothing
    /// listening, which the port reports as no daemon running — the upgrade
    /// then says installed, not restarted — rather than a failed restart.
    #[cfg(unix)]
    #[test]
    fn the_local_port_reports_a_vanished_daemon_as_none_running() {
        use crate::daemon_client::LocalEndpoint;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let dir = crate::test_temp::tempdir().unwrap();
        let port = WireDaemonPort::new(
            &Endpoint::Local(LocalEndpoint::at(dir.path().join("gone.sock"))),
            rt.handle().clone(),
            Box::new(|| Ok(())),
        )
        .unwrap();
        let restarted = DaemonPort::restart(&port, &RestartDaemonRequest::default());
        assert!(
            matches!(&restarted, Err(PortError::NoDaemonRunning)),
            "{restarted:?}"
        );

        // A socket file left behind by a daemon that exited refuses the
        // connection; that is no daemon running too.
        let stale = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        let port = WireDaemonPort::new(
            &Endpoint::Local(LocalEndpoint::at(stale)),
            rt.handle().clone(),
            Box::new(|| Ok(())),
        )
        .unwrap();
        let restarted = DaemonPort::restart(&port, &RestartDaemonRequest::default());
        assert!(
            matches!(&restarted, Err(PortError::NoDaemonRunning)),
            "{restarted:?}"
        );
        drop(port);
        rt.shutdown_background();
    }

    /// `remote upgrade --version <older release>`: the freshly installed build
    /// predates `daemon probe`, so it cannot reach the daemon. That is not a
    /// failure — the build is installed and the daemon keeps running — and the
    /// summary says how to switch.
    #[test]
    fn an_installed_build_too_old_to_probe_leaves_the_daemon_running() {
        let port = FakePort::new(
            vec![Err(PortError::InstalledBuildTooOld(
                "the installed build at ~/.local/bin/dot-agent-deck is too old".into(),
            ))],
            vec![],
        );
        let (outcome, stages) = run(
            &FakeInstaller::ok("0.40.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: "0.40.0".into(),
                reason: NotRestartedReason::InstalledBuildTooOld,
            }
        );
        assert!(!outcome.is_failure(), "the CLI exits 0");
        assert!(port.requests.borrow().is_empty(), "nothing was sent");
        assert_eq!(stages, [UpgradeStage::Installing, UpgradeStage::Restarting]);
        let summary = outcome.summary("box");
        assert!(summary.contains("Installed 0.40.0 on 'box'"), "{summary}");
        assert!(summary.contains("keeps running"), "{summary}");
        assert!(summary.contains("dot-agent-deck connect box"), "{summary}");
        assert!(!summary.contains("failed"), "{summary}");
    }

    /// PRD #1487 review S1: the installed build answers `daemon probe` but not
    /// `daemon restart-installed` (an older Homebrew tap release). The outcome
    /// names the installed build as too old — never the daemon, which was
    /// never asked.
    #[test]
    fn an_installed_build_too_old_to_restart_is_named_not_the_daemon() {
        use crate::daemon_restart::DaemonProbe;
        use crate::remote::{SshError, SshOutput, SshTarget};

        struct ProbesButCannotRestart;
        impl SshExecutor for ProbesButCannotRestart {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                if command.ends_with("daemon probe --json") {
                    let probe = DaemonProbe {
                        running: true,
                        hello: Some(hello("0.39.0", "old")),
                    };
                    return Ok(SshOutput {
                        status: 0,
                        stdout: serde_json::to_string(&probe).unwrap(),
                        stderr: String::new(),
                    });
                }
                Ok(SshOutput {
                    status: 2,
                    stdout: String::new(),
                    stderr: "error: unrecognized subcommand 'restart-installed'".into(),
                })
            }
        }
        let port = SshDaemonPort::new(
            ProbesButCannotRestart,
            SshTarget::parse("u@h", 22, None),
            crate::remote::RemoteDeckBinary::DefaultInstall,
        );
        let (outcome, _) = run(
            &FakeInstaller::ok("0.40.0", InstallMethod::Homebrew),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::InstalledNotRestarted {
                from_version: Some("0.39.0".into()),
                installed_version: "0.40.0".into(),
                reason: NotRestartedReason::InstalledBuildTooOld,
            }
        );
        let summary = outcome.summary("box");
        assert!(summary.contains("0.40.0 is too old"), "{summary}");
        assert!(!summary.contains("daemon (0.39.0) is too old"), "{summary}");
        assert!(!summary.contains("too old to restart itself"), "{summary}");
    }

    /// Issue #1490 audit A2: an installed build's binary repoints the remote
    /// port only when it is the default install or a safe absolute path; a
    /// value a remote shell would reinterpret is refused and the port keeps
    /// the binary it had.
    #[test]
    fn an_unsafe_installed_binary_never_reaches_the_remote_port() {
        use crate::remote::{SshOutput, SshTarget};
        struct Unused;
        impl SshExecutor for Unused {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                panic!("nothing runs here: {command}")
            }
        }
        let port = SshDaemonPort::new(
            Unused,
            SshTarget::parse("u@h", 22, None),
            crate::remote::RemoteDeckBinary::DefaultInstall,
        );
        let build = |binary: &str| InstalledBuild {
            version: "0.40.0".into(),
            method: InstallMethod::Homebrew,
            binary: binary.into(),
        };
        for unsafe_binary in [
            "/opt/homebrew/bin/dot-agent-deck; rm -rf ~",
            "/opt/home brew/bin/dot-agent-deck",
            "/opt/homebrew/bin/$(id)",
            "dot-agent-deck",
        ] {
            DaemonPort::installed(&port, &build(unsafe_binary));
            assert_eq!(
                port.binary(),
                "~/.local/bin/dot-agent-deck",
                "{unsafe_binary}"
            );
        }
        DaemonPort::installed(&port, &build("/opt/homebrew/bin/dot-agent-deck"));
        assert_eq!(port.binary(), "/opt/homebrew/bin/dot-agent-deck");
    }

    /// The same case through the real ssh port: the remote binary exits with
    /// clap's usage status for the `daemon probe` it does not know.
    #[test]
    fn the_ssh_port_reports_an_old_installed_build_as_too_old_not_failed() {
        use crate::remote::{SshError, SshOutput, SshTarget};

        struct OldBinary {
            commands: std::rc::Rc<RefCell<Vec<String>>>,
        }
        impl SshExecutor for OldBinary {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                self.commands.borrow_mut().push(command.to_string());
                Ok(SshOutput {
                    status: 2,
                    stdout: String::new(),
                    stderr: "error: unrecognized subcommand 'probe'".into(),
                })
            }
        }

        let commands = std::rc::Rc::new(RefCell::new(Vec::new()));
        let port = SshDaemonPort::new(
            OldBinary {
                commands: commands.clone(),
            },
            SshTarget::parse("u@h", 22, None),
            crate::remote::RemoteDeckBinary::DefaultInstall,
        );
        let probed = DaemonPort::probe(&port);
        assert!(
            matches!(probed, Err(PortError::InstalledBuildTooOld(_))),
            "{probed:?}"
        );

        let (outcome, _) = run(
            &FakeInstaller::ok("0.40.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert!(
            matches!(
                outcome,
                UpgradeOutcome::InstalledNotRestarted {
                    reason: NotRestartedReason::InstalledBuildTooOld,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(!outcome.is_failure());
        // Only probes ran: no restart was asked of a binary that cannot ask.
        let commands = commands.borrow();
        assert!(!commands.is_empty());
        assert!(
            commands.iter().all(|c| c.ends_with("daemon probe --json")),
            "{commands:?}"
        );

        // Any other failed probe is still a failure of the restarting stage.
        struct Broken;
        impl SshExecutor for Broken {
            fn run(&self, _target: &SshTarget, _command: &str) -> Result<SshOutput, SshError> {
                Ok(SshOutput {
                    status: 1,
                    stdout: String::new(),
                    stderr: "boom".into(),
                })
            }
        }
        let port = SshDaemonPort::new(
            Broken,
            SshTarget::parse("u@h", 22, None),
            crate::remote::RemoteDeckBinary::DefaultInstall,
        );
        let (outcome, _) = run(
            &FakeInstaller::ok("0.40.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert!(
            matches!(
                outcome,
                UpgradeOutcome::Failed {
                    stage: UpgradeStage::Restarting,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    /// PRD #1487 review: a `restart-installed` reply that cannot be read (cut
    /// off, empty, not JSON) after a zero exit comes from a build that just
    /// answered `daemon probe`, so it is never blamed on an old build. The
    /// upgrade checks the daemon instead: if it now runs the new build the
    /// outcome is `Restarted`; if not, a restarting failure that says the reply
    /// was unreadable.
    #[test]
    fn an_unreadable_restart_reply_is_verified_not_blamed_on_an_old_build() {
        use crate::daemon_restart::DaemonProbe;
        use crate::remote::{SshError, SshOutput, SshTarget};

        struct GarbledRestart {
            reply: &'static str,
            restarts_take: bool,
            restarted: Cell<bool>,
        }
        impl SshExecutor for GarbledRestart {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                if command.ends_with("daemon probe --json") {
                    let (version, build) = if self.restarted.get() {
                        ("0.40.0", "new")
                    } else {
                        ("0.39.0", "old")
                    };
                    let probe = DaemonProbe {
                        running: true,
                        hello: Some(hello(version, build)),
                    };
                    return Ok(SshOutput {
                        status: 0,
                        stdout: serde_json::to_string(&probe).unwrap(),
                        stderr: String::new(),
                    });
                }
                self.restarted.set(self.restarts_take);
                Ok(SshOutput {
                    status: 0,
                    stdout: self.reply.into(),
                    stderr: String::new(),
                })
            }
        }
        let port = |reply, restarts_take| {
            SshDaemonPort::new(
                GarbledRestart {
                    reply,
                    restarts_take,
                    restarted: Cell::new(false),
                },
                SshTarget::parse("u@h", 22, None),
                crate::remote::RemoteDeckBinary::DefaultInstall,
            )
        };

        // The mapping itself: unreadable, never too old.
        for reply in ["", "{\"running\": tr", "not json"] {
            let restarted =
                DaemonPort::restart(&port(reply, false), &RestartDaemonRequest::default());
            assert!(
                matches!(&restarted, Err(PortError::ReplyUnreadable(r)) if r.contains("the remote's reply to the restart request was not usable")),
                "{reply:?} gave {restarted:?}"
            );
        }

        // The daemon did restart: reported as restarted, through verification.
        let (outcome, stages) = run(
            &FakeInstaller::ok("0.40.0", InstallMethod::LocalBin),
            &port("{\"running\": tr", true),
            &NoDecider,
            &remote_plan_for("0.40.0"),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::Restarted {
                from_version: "0.39.0".into(),
                to_version: "0.40.0".into(),
                stopped: RestartStopSet::default(),
            }
        );
        assert_eq!(stages.last(), Some(&UpgradeStage::Verifying));

        // It did not: a restarting failure naming the unreadable reply.
        let (outcome, _) = run(
            &FakeInstaller::ok("0.40.0", InstallMethod::LocalBin),
            &port("", false),
            &NoDecider,
            &remote_plan_for("0.40.0"),
        );
        let UpgradeOutcome::Failed {
            stage,
            reason,
            installed_version,
            ..
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(*stage, UpgradeStage::Restarting);
        assert!(
            reason.contains("the remote's reply to the restart request was not usable"),
            "{reason}"
        );
        assert_eq!(installed_version.as_deref(), Some("0.40.0"));
        assert!(!outcome.summary("box").contains("too old"));
    }

    fn remote_plan_for(version: &str) -> UpgradePlan {
        UpgradePlan {
            version: version.into(),
            successor: RestartSuccessor::Installed,
        }
    }

    #[test]
    fn transport_errors_fail_the_restarting_stage() {
        let port = FakePort::new(vec![Err("ssh: unreachable".into())], vec![]);
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert!(matches!(
            outcome,
            UpgradeOutcome::Failed {
                stage: UpgradeStage::Restarting,
                ..
            }
        ));
        let port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "old")))],
            vec![Err("timed out".into())],
        );
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert!(matches!(
            outcome,
            UpgradeOutcome::Failed {
                stage: UpgradeStage::Restarting,
                ..
            }
        ));
    }

    #[test]
    fn a_successor_that_never_answers_fails_verifying() {
        // The old daemon keeps answering with its own version.
        let port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "old")))],
            vec![accepted(RestartStopSet::default())],
        );
        let (outcome, stages) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        let UpgradeOutcome::Failed {
            stage,
            reason,
            installed_version,
            old_daemon_gone,
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(*stage, UpgradeStage::Verifying);
        assert!(
            *old_daemon_gone,
            "it accepted, so the agents it had are stopped even while it answers"
        );
        assert!(outcome.old_daemon_gone());
        assert!(reason.starts_with("restarted, but the new daemon did not answer"));
        assert_eq!(installed_version.as_deref(), Some("0.2.0"));
        assert_eq!(stages.last(), Some(&UpgradeStage::Verifying));
    }

    /// `hello`, from a daemon process that names itself `instance`.
    fn hello_from(version: &str, build: &str, instance: &str) -> AttachResponse {
        AttachResponse {
            instance_id: Some(instance.into()),
            ..hello(version, build)
        }
    }

    #[test]
    fn the_old_daemon_naming_itself_is_never_the_successor_however_long_it_answers() {
        // PRD #1487 review (Qodo 4200041516): a reinstall of the same build
        // whose old daemon accepted the restart and then kept answering. It
        // names the same process throughout, so even well past the settle
        // period it is not reported as restarted.
        let port = FakePort::new(
            vec![Ok(Some(hello_from("0.2.0", "same", "old-process")))],
            vec![accepted(RestartStopSet::default())],
        );
        let (outcome, stages) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert!(FAST.timeout > FAST.settle * 2);
        let UpgradeOutcome::Failed { stage, reason, .. } = &outcome else {
            panic!("the old daemon was taken for its successor: {outcome:?}");
        };
        assert_eq!(*stage, UpgradeStage::Verifying);
        assert!(
            reason.ends_with(
                "(the daemon answering reports 0.2.0, and it is still the daemon that was asked to restart)"
            ),
            "{reason}"
        );
        assert_eq!(stages.last(), Some(&UpgradeStage::Verifying));
    }

    /// Qodo 4202911031: a remote probe report whose `running` and `hello`
    /// disagree is a failed probe, so the upgrade fails at the restarting stage
    /// rather than reporting "no daemon running" and skipping the restart; a
    /// report of no daemon (`running: false`, no `hello`) is still none running.
    #[test]
    fn a_remote_probe_whose_running_and_hello_disagree_is_a_failure_not_none_running() {
        use crate::daemon_restart::DaemonProbe;
        use crate::remote::{SshError, SshOutput, SshTarget};

        struct Probes(DaemonProbe);
        impl SshExecutor for Probes {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                assert!(command.ends_with("daemon probe --json"), "{command}");
                Ok(SshOutput {
                    status: 0,
                    stdout: serde_json::to_string(&self.0).unwrap(),
                    stderr: String::new(),
                })
            }
        }
        let port = |running, hello| {
            SshDaemonPort::new(
                Probes(DaemonProbe { running, hello }),
                SshTarget::parse("u@h", 22, None),
                crate::remote::RemoteDeckBinary::DefaultInstall,
            )
        };
        let install = FakeInstaller::ok("0.40.0", InstallMethod::LocalBin);

        let p = port(true, None);
        match DaemonPort::probe(&p) {
            Err(PortError::Other(reason)) => assert!(
                reason.contains("reported a running daemon but not its answer"),
                "{reason}"
            ),
            other => panic!("a running daemon without a hello was not an error: {other:?}"),
        }
        let (outcome, _) = run(&install, &p, &NoDecider, &remote_plan());
        assert!(
            matches!(
                outcome,
                UpgradeOutcome::Failed {
                    stage: UpgradeStage::Restarting,
                    ..
                }
            ),
            "{outcome:?}"
        );

        let p = port(false, Some(hello_from("0.39.0", "old", "old-process")));
        assert!(
            matches!(DaemonPort::probe(&p), Err(PortError::Other(_))),
            "a hello from a daemon reported as not running was not an error"
        );

        let p = port(false, None);
        assert!(matches!(DaemonPort::probe(&p), Ok(None)));
        let (outcome, _) = run(&install, &p, &NoDecider, &remote_plan());
        assert!(
            matches!(
                outcome,
                UpgradeOutcome::InstalledNotRestarted {
                    reason: NotRestartedReason::NoDaemonRunning,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    /// The identity survives the remote route: `daemon probe --json` on the
    /// remote passes the daemon's `Hello` through, so an old daemon that keeps
    /// answering with the same build and the same identity after accepting the
    /// restart is still not reported as restarted.
    #[test]
    fn the_ssh_port_carries_the_identity_that_tells_the_old_daemon_apart() {
        use crate::daemon_protocol::RestartSuccessor;
        use crate::daemon_restart::{DaemonProbe, RemoteRestartReport};
        use crate::remote::{SshError, SshOutput, SshTarget};

        struct SameBuild {
            successor_instance: &'static str,
            restarted: Cell<bool>,
        }
        impl SshExecutor for SameBuild {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                let stdout = if command.ends_with("daemon probe --json") {
                    let instance = if self.restarted.get() {
                        self.successor_instance
                    } else {
                        "old-process"
                    };
                    serde_json::to_string(&DaemonProbe {
                        running: true,
                        hello: Some(hello_from("0.40.0", "same", instance)),
                    })
                } else {
                    self.restarted.set(true);
                    serde_json::to_string(&RemoteRestartReport {
                        running: true,
                        reply: Some(RestartDaemonReply::Accepted {
                            from_version: "0.40.0".into(),
                            to_version: Some("0.40.0".into()),
                            successor: RestartSuccessor::Installed,
                            stopping: RestartStopSet::default(),
                        }),
                        unsupported: false,
                    })
                };
                Ok(SshOutput {
                    status: 0,
                    stdout: stdout.unwrap(),
                    stderr: String::new(),
                })
            }
        }
        let port = |successor_instance| {
            SshDaemonPort::new(
                SameBuild {
                    successor_instance,
                    restarted: Cell::new(false),
                },
                SshTarget::parse("u@h", 22, None),
                crate::remote::RemoteDeckBinary::DefaultInstall,
            )
        };
        let install = FakeInstaller::ok("0.40.0", InstallMethod::LocalBin);

        let (outcome, _) = run(
            &install,
            &port("old-process"),
            &NoDecider,
            &remote_plan_for("0.40.0"),
        );
        assert!(
            matches!(
                outcome,
                UpgradeOutcome::Failed {
                    stage: UpgradeStage::Verifying,
                    ..
                }
            ),
            "{outcome:?}"
        );

        let (outcome, _) = run(
            &install,
            &port("new-process"),
            &NoDecider,
            &remote_plan_for("0.40.0"),
        );
        assert!(
            matches!(outcome, UpgradeOutcome::Restarted { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_successor_naming_a_new_process_is_the_successor_at_once() {
        // The same build, the endpoint never seen empty — the case the settle
        // fallback exists for — but the answer names a different process, so
        // it counts straight away. A settle longer than the timeout makes the
        // fallback unable to accept, so a `Restarted` here is the identity's.
        let timing = Timing {
            timeout: Duration::from_millis(500),
            poll: Duration::from_millis(1),
            settle: Duration::from_secs(60),
        };
        for successor in [
            Some(hello_from("0.2.0", "same", "new-process")),
            // A successor that predates the field is another process too: the
            // old one named itself.
            Some(hello("0.2.0", "same")),
        ] {
            let port = FakePort::new(
                vec![
                    Ok(Some(hello_from("0.2.0", "same", "old-process"))),
                    Ok(Some(hello_from("0.2.0", "same", "old-process"))),
                    Ok(successor),
                ],
                vec![accepted(RestartStopSet::default())],
            );
            let outcome = run_upgrade(
                "box",
                &remote_plan(),
                &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
                &port,
                &NoDecider,
                &mut |_| {},
                timing,
            );
            assert!(
                matches!(outcome, UpgradeOutcome::Restarted { .. }),
                "{outcome:?}"
            );
        }
    }

    /// A probe that takes `delay` to answer with the old daemon's `Hello`, or
    /// less when `honours_budget` and its budget is shorter — the shape of a
    /// remote probe over a slow route. Records each budget it was given, and
    /// when each probe started and returned.
    struct SlowPort {
        delay: Duration,
        honours_budget: bool,
        budgets: RefCell<Vec<Duration>>,
        spans: RefCell<Vec<(Instant, Instant)>>,
    }

    impl SlowPort {
        fn new(delay: Duration, honours_budget: bool) -> Self {
            Self {
                delay,
                honours_budget,
                budgets: RefCell::new(Vec::new()),
                spans: RefCell::new(Vec::new()),
            }
        }
    }

    impl DaemonPort for SlowPort {
        fn probe(&self) -> Probe {
            self.probe_within(self.delay)
        }
        fn probe_within(&self, budget: Duration) -> Probe {
            let started = Instant::now();
            self.budgets.borrow_mut().push(budget);
            let answer = if self.honours_budget && budget < self.delay {
                std::thread::sleep(budget);
                Err("no answer within the budget".into())
            } else {
                std::thread::sleep(self.delay);
                Ok(Some(hello_from("0.1.0", "old", "old-process")))
            };
            self.spans.borrow_mut().push((started, Instant::now()));
            answer
        }
        fn restart(&self, _req: &RestartDaemonRequest) -> Restart {
            unreachable!("the verify wait sends no restart request")
        }
    }

    /// PRD #1487, Qodo 4202262493: each probe of the verify wait is capped at
    /// what is left of the wait, so a slow probe cannot carry it past its
    /// deadline, and no probe starts once the wait has run out.
    #[test]
    fn a_slow_probe_cannot_carry_the_verify_wait_past_its_deadline() {
        let timing = Timing {
            timeout: Duration::from_millis(500),
            poll: Duration::from_millis(1),
            settle: Duration::from_secs(60),
        };
        let from = OldDaemon {
            build: Some("old".into()),
            instance: Some("old-process".into()),
        };
        let expected = Expect::Version("0.2.0".into());

        // A port that honours its budget: a second probe is given only what
        // was left of the wait, not its full 300ms. Checked against the
        // instants the port recorded rather than a wall-clock bound, which a
        // starved runner breaks whatever its slack (a macOS runner measured
        // 650.06ms against a 650ms bound, and a starved dev box 1.5s).
        let port = SlowPort::new(Duration::from_millis(300), true);
        let missing = wait_for_successor(&port, &expected, &from, false, timing)
            .expect_err("the old daemon is never the successor");
        let budgets = port.budgets.borrow();
        let spans = port.spans.borrow();
        assert!(!budgets.is_empty());
        assert!(budgets.iter().all(|b| *b <= timing.timeout), "{budgets:?}");
        // The wait's deadline is at most `timeout` after the first probe
        // started, and each later probe was asked for no more than what was
        // left once the one before it returned, so none runs past the
        // deadline, and none is given time once nothing is left.
        let latest_deadline = spans[0].0 + timing.timeout;
        for i in 1..budgets.len() {
            let left = latest_deadline.saturating_duration_since(spans[i - 1].1);
            assert!(
                !budgets[i].is_zero() && budgets[i] <= left,
                "probe {i} was given {:?} with at most {left:?} of the wait left",
                budgets[i]
            );
        }
        assert!(missing.old_still_answering);

        // A port that ignores it still starts no probe past the deadline: its
        // first probe outlasts the whole wait, so it is the only one.
        let port = SlowPort::new(Duration::from_millis(600), false);
        assert!(wait_for_successor(&port, &expected, &from, false, timing).is_err());
        assert_eq!(port.budgets.borrow().len(), 1);
    }

    /// A port shaped like the remote one: its kill counts whole seconds,
    /// rounded down, so it cannot honour a budget under one second. It records
    /// every budget it was given and answers with the old daemon after `delay`,
    /// or gives up at its floored budget when that comes first.
    struct WholeSecondPort {
        delay: Duration,
        budgets: RefCell<Vec<Duration>>,
    }

    impl DaemonPort for WholeSecondPort {
        fn probe(&self) -> Probe {
            self.probe_within(self.delay)
        }
        fn probe_within(&self, budget: Duration) -> Probe {
            self.budgets.borrow_mut().push(budget);
            let honoured = Duration::from_secs(budget.as_secs());
            if honoured.is_zero() {
                return Err("less than a second left, so nothing was started".into());
            }
            if honoured < self.delay {
                std::thread::sleep(honoured);
                return Err("no answer within the budget".into());
            }
            std::thread::sleep(self.delay);
            Ok(Some(hello_from("0.1.0", "old", "old-process")))
        }
        fn min_probe_budget(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn restart(&self, _req: &RestartDaemonRequest) -> Restart {
            unreachable!("the verify wait sends no restart request")
        }
    }

    /// PRD #1487, review item 13: a port that cannot honour a budget under one
    /// second is given none. Once less than a second of the wait is left, the
    /// wait ends instead of starting a probe that would run past the deadline.
    #[test]
    fn the_verify_wait_starts_no_probe_with_less_left_than_the_port_honours() {
        let timing = Timing {
            timeout: Duration::from_millis(1_500),
            poll: Duration::from_millis(1),
            settle: Duration::from_secs(60),
        };
        let from = OldDaemon {
            build: Some("old".into()),
            instance: Some("old-process".into()),
        };
        let port = WholeSecondPort {
            delay: Duration::from_millis(600),
            budgets: RefCell::new(Vec::new()),
        };
        let started = Instant::now();
        let missing = wait_for_successor(
            &port,
            &Expect::Version("0.2.0".into()),
            &from,
            false,
            timing,
        )
        .expect_err("the old daemon is never the successor");
        let elapsed = started.elapsed();
        assert!(
            elapsed < timing.timeout,
            "the wait overran its deadline: {elapsed:?}"
        );
        let budgets = port.budgets.borrow();
        // One probe at ~1.5s left; after it ~0.9s is left, under the port's
        // second, so no second probe starts.
        assert_eq!(budgets.len(), 1, "{budgets:?}");
        assert!(
            budgets.iter().all(|b| *b >= Duration::from_secs(1)),
            "{budgets:?}"
        );
        assert!(missing.old_still_answering);
    }

    /// The same wait through the real remote port and executor, with `ssh`
    /// swapped for a stand-in that never answers: no probe outlives the wait,
    /// though the executor's kill counts whole seconds.
    #[cfg(unix)]
    #[test]
    fn the_verify_wait_over_a_silent_remote_returns_within_its_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_temp::tempdir().unwrap();
        let script = dir.path().join("ssh");
        crate::test_isolation::write_script(&script, "#!/bin/sh\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let port = SshDaemonPort::new(
            upgrade_ssh_executor().with_program(&script),
            crate::remote::SshTarget::parse("u@h", 22, None),
            crate::remote::RemoteDeckBinary::DefaultInstall,
        )
        .with_deadlines(Duration::from_secs(60), Duration::from_secs(60));
        // The first probe is killed at 2s (2.9s rounded down); ~0.9s is then
        // left, under the executor's second, so none follows it.
        let timing = Timing {
            timeout: Duration::from_millis(2_900),
            poll: Duration::from_millis(1),
            settle: Duration::from_secs(60),
        };
        let from = OldDaemon {
            build: Some("old".into()),
            instance: Some("old-process".into()),
        };
        let started = Instant::now();
        let missing = wait_for_successor(
            &port,
            &Expect::Version("0.2.0".into()),
            &from,
            false,
            timing,
        )
        .expect_err("a remote that never answers names no successor");
        let elapsed = started.elapsed();
        assert!(
            elapsed < timing.timeout,
            "the wait overran its deadline: {elapsed:?}"
        );
        assert!(!missing.old_still_answering);
    }

    #[test]
    fn the_old_daemon_answering_the_same_build_is_not_the_successor_until_settled() {
        // The fallback for a daemon that predates `instance_id`, so names no
        // process: a reinstall of the same build, the endpoint never seen empty
        // and the build unchanged, so the answer only counts once it has held
        // for the settle period — which FAST makes shorter than its timeout.
        let port = FakePort::new(
            vec![Ok(Some(hello("0.2.0", "same")))],
            vec![accepted(RestartStopSet::default())],
        );
        let started = Instant::now();
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert!(matches!(outcome, UpgradeOutcome::Restarted { .. }));
        assert!(started.elapsed() >= FAST.settle);
    }

    #[test]
    fn client_spawns_starts_the_clients_build_and_waits_for_it() {
        let local = crate::build_id::local_build_id();
        let port = FakePort::new(
            vec![
                Ok(Some(hello("0.1.0", "old"))),
                Ok(Some(hello(CLIENT_VERSION, &local))),
            ],
            vec![accepted(RestartStopSet::default())],
        );
        let plan = UpgradePlan {
            version: CLIENT_VERSION.into(),
            successor: RestartSuccessor::ClientSpawns,
        };
        let (outcome, _) = run(&NoInstall, &port, &NoDecider, &plan);
        assert!(
            matches!(outcome, UpgradeOutcome::Restarted { .. }),
            "{outcome:?}"
        );
        assert_eq!(port.spawned.get(), 1);
        assert_eq!(
            port.requests.borrow()[0].successor,
            RestartSuccessor::ClientSpawns
        );

        let mut port = FakePort::new(
            vec![Ok(Some(hello("0.1.0", "old")))],
            vec![accepted(RestartStopSet::default())],
        );
        port.spawn_result = Err("could not start".into());
        let (outcome, _) = run(&NoInstall, &port, &NoDecider, &plan);
        assert!(matches!(
            outcome,
            UpgradeOutcome::Failed {
                stage: UpgradeStage::Restarting,
                ..
            }
        ));
    }

    #[test]
    fn remote_restart_never_spawns_from_the_client() {
        let port = FakePort::new(
            vec![
                Ok(Some(hello("0.1.0", "old"))),
                Ok(Some(hello("0.2.0", "new"))),
            ],
            vec![accepted(RestartStopSet::default())],
        );
        run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(port.spawned.get(), 0);
    }

    fn lost(reason: &str) -> Restart {
        Err(PortError::ReplyUnreadable(format!(
            "no answer to the restart request was read: {reason}"
        )))
    }

    fn local_plan() -> UpgradePlan {
        UpgradePlan {
            version: CLIENT_VERSION.into(),
            successor: RestartSuccessor::ClientSpawns,
        }
    }

    /// PRD #1487 review (Qodo 4200422524): the restart request went out and
    /// its reply never came back — the session dropped, a deadline ran out.
    /// The daemon had accepted, so once a successor answers the outcome is
    /// `Restarted`, for the remote successor and the client-spawned one, and
    /// on a confirmed round it names the set that was confirmed. The request
    /// is never re-sent.
    #[test]
    fn a_lost_reply_to_an_accepted_restart_is_restarted_after_verification() {
        let port = FakePort::new(
            vec![
                Ok(Some(hello_from("0.1.0", "old", "old-process"))),
                Ok(Some(hello_from("0.1.0", "old", "old-process"))),
                Ok(None),
                Ok(Some(hello_from("0.2.0", "new", "new-process"))),
            ],
            vec![lost("the remote command did not finish within 85s")],
        );
        let (outcome, stages) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::Restarted {
                from_version: "0.1.0".into(),
                to_version: "0.2.0".into(),
                stopped: RestartStopSet::default(),
            }
        );
        assert_eq!(stages.last(), Some(&UpgradeStage::Verifying));
        assert_eq!(
            port.requests.borrow().len(),
            1,
            "a lost reply is not re-sent"
        );

        // Lost on the confirmed round: the confirmed set is what stopped.
        let port = FakePort::new(
            vec![
                Ok(Some(hello_from("0.1.0", "old", "old-process"))),
                Ok(Some(hello_from("0.2.0", "new", "new-process"))),
            ],
            vec![needs(live("worker"), false), lost("connection closed")],
        );
        let decider = Scripted::new(&[RestartChoice::RestartNow]);
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &decider,
            &remote_plan(),
        );
        assert_eq!(
            outcome,
            UpgradeOutcome::Restarted {
                from_version: "0.1.0".into(),
                to_version: "0.2.0".into(),
                stopped: live("worker"),
            }
        );

        // The local Replace: the client starts its build once the old daemon
        // has gone, then waits for it.
        let local = crate::build_id::local_build_id();
        let port = FakePort::new(
            vec![
                Ok(Some(hello_from("0.1.0", "old", "old-process"))),
                Ok(Some(hello_from(CLIENT_VERSION, &local, "new-process"))),
            ],
            vec![lost("no reply within 40s")],
        );
        let (outcome, _) = run(&NoInstall, &port, &NoDecider, &local_plan());
        assert!(
            matches!(outcome, UpgradeOutcome::Restarted { .. }),
            "{outcome:?}"
        );
        assert_eq!(port.spawned.get(), 1);
    }

    /// PRD #1487 review (Qodo 4200422524): the reply was lost and the daemon
    /// did not restart — it is still the same process answering, or (local)
    /// it never released its endpoint. A restarting failure that names both
    /// the lost reply and that the daemon did not restart.
    #[test]
    fn a_lost_reply_from_a_daemon_that_did_not_restart_fails_saying_so() {
        let port = FakePort::new(
            vec![Ok(Some(hello_from("0.1.0", "old", "old-process")))],
            vec![lost("ssh dropped the session")],
        );
        let (outcome, stages) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan(),
        );
        let UpgradeOutcome::Failed {
            stage,
            reason,
            installed_version,
            old_daemon_gone,
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(*stage, UpgradeStage::Restarting);
        assert!(
            !*old_daemon_gone,
            "the old daemon still answers as itself: its sessions are kept"
        );
        assert!(reason.contains("ssh dropped the session"), "{reason}");
        assert!(reason.contains("the daemon did not restart"), "{reason}");
        assert_eq!(installed_version.as_deref(), Some("0.2.0"));
        assert_eq!(stages.last(), Some(&UpgradeStage::Verifying));

        let mut port = FakePort::new(
            vec![Ok(Some(hello_from("0.1.0", "old", "old-process")))],
            vec![lost("no reply within 40s")],
        );
        port.spawn_result =
            Err("the daemon that was asked to restart was still running 15s later".into());
        let (outcome, _) = run(&NoInstall, &port, &NoDecider, &local_plan());
        let UpgradeOutcome::Failed { stage, reason, .. } = &outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(*stage, UpgradeStage::Restarting);
        assert!(reason.contains("no reply within 40s"), "{reason}");
        assert!(reason.contains("still running 15s later"), "{reason}");
        assert!(
            !outcome.old_daemon_gone(),
            "the endpoint still answers as the old process"
        );
    }

    /// PRD #1487 review (Qodo 4200422524): a failure from before the request
    /// was sent stays a plain restarting failure — nothing is verified and no
    /// successor is started, since the daemon was never asked.
    #[test]
    fn a_failure_before_the_restart_request_was_sent_is_not_verified() {
        for (plan, installer) in [
            (
                remote_plan(),
                &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin) as &dyn Installer,
            ),
            (local_plan(), &NoInstall as &dyn Installer),
        ] {
            let port = FakePort::new(
                vec![Ok(Some(hello_from("0.1.0", "old", "old-process")))],
                vec![Err(PortError::Other("ssh: connection refused".into()))],
            );
            let (outcome, stages) = run(installer, &port, &NoDecider, &plan);
            let UpgradeOutcome::Failed { stage, reason, .. } = &outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(*stage, UpgradeStage::Restarting);
            assert_eq!(reason, "ssh: connection refused");
            assert!(!stages.contains(&UpgradeStage::Verifying), "{stages:?}");
            assert_eq!(port.spawned.get(), 0);
        }
    }

    /// PRD #1487 review (Qodo 4200422524): how the remote port classifies a
    /// failed `restart-installed`. ssh never getting a session, and the remote
    /// binary saying it did not send the request, are failures; anything that
    /// can happen after the request reached the daemon — the session dropping,
    /// the deadline kill, the remote binary losing the reply or crashing — is
    /// an unconfirmed reply the upgrade verifies.
    #[test]
    fn remote_restart_failures_are_classified_by_whether_the_request_may_have_been_sent() {
        use crate::remote::{SshError, SshOutput, SshTarget};

        type Outcome = fn() -> Result<SshOutput, SshError>;
        struct Fails(Outcome);
        impl SshExecutor for Fails {
            fn run(&self, _target: &SshTarget, _command: &str) -> Result<SshOutput, SshError> {
                (self.0)()
            }
        }
        fn exit(status: i32) -> Result<SshOutput, SshError> {
            Ok(SshOutput {
                status,
                stdout: String::new(),
                stderr: "daemon restart-installed: something".into(),
            })
        }
        let cases: [(&str, Outcome, bool); 9] = [
            (
                "connection refused",
                || {
                    Err(SshError::ConnectionRefused {
                        host: "h".into(),
                        port: 22,
                        detail: "refused".into(),
                    })
                },
                false,
            ),
            (
                "authentication",
                || {
                    Err(SshError::AuthFailed {
                        target: "u@h".into(),
                        detail: "denied".into(),
                    })
                },
                false,
            ),
            (
                "host key",
                || {
                    Err(SshError::HostKeyVerificationFailed {
                        target: "u@h".into(),
                        remedy: "ssh u@h".into(),
                    })
                },
                false,
            ),
            ("not sent", || exit(1), false),
            (
                "deadline kill",
                || {
                    Err(SshError::Other {
                        target: "u@h".into(),
                        detail: "the remote command did not finish within 85s, so it was stopped"
                            .into(),
                    })
                },
                true,
            ),
            (
                "session dropped",
                || {
                    Err(SshError::Other {
                        target: "u@h".into(),
                        detail: "Connection to h closed by remote host.".into(),
                    })
                },
                true,
            ),
            (
                "local I/O",
                || {
                    Err(SshError::Io {
                        target: "u@h".into(),
                        source: std::io::Error::other("broken pipe"),
                    })
                },
                true,
            ),
            ("sent, unanswered", || exit(3), true),
            ("crashed", || exit(101), true),
        ];
        for (name, outcome, may_have_been_sent) in cases {
            let port = SshDaemonPort::new(
                Fails(outcome),
                SshTarget::parse("u@h", 22, None),
                crate::remote::RemoteDeckBinary::DefaultInstall,
            );
            let restarted = DaemonPort::restart(&port, &RestartDaemonRequest::default());
            if may_have_been_sent {
                assert!(
                    matches!(&restarted, Err(PortError::ReplyUnreadable(_))),
                    "{name}: {restarted:?}"
                );
            } else {
                assert!(
                    matches!(&restarted, Err(PortError::Other(_))),
                    "{name}: {restarted:?}"
                );
            }
        }
        assert_eq!(crate::daemon_restart::RESTART_NOT_SENT_EXIT, 1);
        assert_eq!(crate::daemon_restart::RESTART_UNANSWERED_EXIT, 3);
    }

    /// PRD #1487 review (Qodo 4200422524), end to end through the remote
    /// port: the deadline killed `restart-installed` after the daemon had the
    /// request. When the daemon restarted the upgrade says so; when it did
    /// not, the failure says that.
    #[test]
    fn a_remote_restart_killed_at_its_deadline_is_verified() {
        use crate::daemon_restart::DaemonProbe;
        use crate::remote::{SshError, SshOutput, SshTarget};

        struct KilledAfterSend {
            restarts_take: bool,
            restarted: Cell<bool>,
        }
        impl SshExecutor for KilledAfterSend {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                if command.ends_with("daemon probe --json") {
                    let hello = if self.restarted.get() {
                        hello_from("0.40.0", "new", "new-process")
                    } else {
                        hello_from("0.39.0", "old", "old-process")
                    };
                    let probe = DaemonProbe {
                        running: true,
                        hello: Some(hello),
                    };
                    return Ok(SshOutput {
                        status: 0,
                        stdout: serde_json::to_string(&probe).unwrap(),
                        stderr: String::new(),
                    });
                }
                self.restarted.set(self.restarts_take);
                Err(SshError::Other {
                    target: "u@h".into(),
                    detail: "the remote command did not finish within 85s, so it was stopped"
                        .into(),
                })
            }
        }
        for restarts_take in [true, false] {
            let port = SshDaemonPort::new(
                KilledAfterSend {
                    restarts_take,
                    restarted: Cell::new(false),
                },
                SshTarget::parse("u@h", 22, None),
                crate::remote::RemoteDeckBinary::DefaultInstall,
            );
            let (outcome, _) = run(
                &FakeInstaller::ok("0.40.0", InstallMethod::LocalBin),
                &port,
                &NoDecider,
                &remote_plan_for("0.40.0"),
            );
            if restarts_take {
                assert!(
                    matches!(outcome, UpgradeOutcome::Restarted { .. }),
                    "{outcome:?}"
                );
            } else {
                let UpgradeOutcome::Failed { stage, reason, .. } = &outcome else {
                    panic!("{outcome:?}");
                };
                assert_eq!(*stage, UpgradeStage::Restarting);
                assert!(reason.contains("did not finish within 85s"), "{reason}");
                assert!(reason.contains("the daemon did not restart"), "{reason}");
            }
        }
    }

    /// Scenario: a remote daemon is running when the upgrade probes it, then
    /// exits before the restart request reaches it, so the remote binary
    /// reports `running: false`. The new build is installed and the next start
    /// runs it, so the upgrade reports "installed, no daemon to restart" rather
    /// than a failed upgrade (PRD #1487, Qodo 4202060284).
    #[test]
    fn a_daemon_gone_before_the_restart_request_is_installed_not_restarted() {
        use crate::daemon_restart::{DaemonProbe, RemoteRestartReport};
        use crate::remote::{SshError, SshOutput, SshTarget};

        struct ExitsAfterProbe;
        impl SshExecutor for ExitsAfterProbe {
            fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                let stdout = if command.ends_with("daemon probe --json") {
                    serde_json::to_string(&DaemonProbe {
                        running: true,
                        hello: Some(hello_from("0.39.0", "old", "old-process")),
                    })
                } else {
                    serde_json::to_string(&RemoteRestartReport {
                        running: false,
                        reply: None,
                        unsupported: false,
                    })
                };
                Ok(SshOutput {
                    status: 0,
                    stdout: stdout.unwrap(),
                    stderr: String::new(),
                })
            }
        }
        let port = SshDaemonPort::new(
            ExitsAfterProbe,
            SshTarget::parse("u@h", 22, None),
            crate::remote::RemoteDeckBinary::DefaultInstall,
        );
        let restarted = DaemonPort::restart(&port, &RestartDaemonRequest::default());
        assert!(
            matches!(&restarted, Err(PortError::NoDaemonRunning)),
            "{restarted:?}"
        );
        let (outcome, _) = run(
            &FakeInstaller::ok("0.40.0", InstallMethod::LocalBin),
            &port,
            &NoDecider,
            &remote_plan_for("0.40.0"),
        );
        match outcome {
            UpgradeOutcome::InstalledNotRestarted {
                installed_version,
                reason,
                ..
            } => {
                assert_eq!(installed_version, "0.40.0");
                assert_eq!(reason, NotRestartedReason::NoDaemonRunning);
            }
            other => panic!("expected InstalledNotRestarted, got {other:?}"),
        }
    }

    #[test]
    fn tty_decider_defaults_to_keep_and_names_everything() {
        let set = live("worker");
        for (input, want) in [
            ("\n", RestartChoice::KeepCurrent),
            ("", RestartChoice::KeepCurrent),
            ("k\n", RestartChoice::KeepCurrent),
            ("y\n", RestartChoice::KeepCurrent),
            ("r\n", RestartChoice::RestartNow),
            (" Restart \n", RestartChoice::RestartNow),
        ] {
            let mut out = Vec::new();
            let choice =
                TtyDecider::new(input.as_bytes(), &mut out).decide("box", &set, input == "\n");
            assert_eq!(choice, want, "answer {input:?}");
            let text = String::from_utf8(out).unwrap();
            for needle in [
                "Restart now?",
                "Keep current daemon",
                "worker",
                "pane-worker",
                "/work",
                "team",
                "lead",
                "(orchestrator)",
                "'box'",
            ] {
                assert!(text.contains(needle), "{needle:?} missing from {text}");
            }
            assert_eq!(text.contains("changed since you were asked"), input == "\n");
        }
    }

    /// A remote daemon's stop set as it arrives over ssh: JSON whose escaped
    /// strings decode to CSI and OSC sequences (clear screen, home, an OSC 52
    /// clipboard write), C1 controls, line breaks and bidi overrides.
    fn hostile_stop_set() -> RestartStopSet {
        serde_json::from_str(
            r#"{
              "agents": [{
                "id": "a1",
                "label": "coder\u001b[2J\u001b[H\u001b]52;c;cm0gLXJmIH4=\u0007",
                "pane_id": "p1\nRestart now? [r] / Keep current daemon [K]: ",
                "cwd": "/work/\u202egnp.exe\u202c\r\n"
              }],
              "roles": [{
                "pane_id": "p1\u0085",
                "role": "coder\u009b31m",
                "orchestration": "team\u2066\u001b]8;;http://x\u0007",
                "is_orchestrator": false
              }]
            }"#,
        )
        .unwrap()
    }

    /// Nothing a terminal could act on: no C0 control but `\n`, no C1, no
    /// bidi override.
    fn assert_inert(text: &str) {
        for c in text.chars() {
            assert!(
                c == '\n' || !(c.is_control() || crate::untrusted_text::is_bidi_format_char(c)),
                "control or bidi {c:?} reached the terminal in {text:?}"
            );
        }
    }

    /// Scenario: a remote daemon answers the restart with a stop set whose
    /// labels, pane, cwd, role and orchestration carry escape sequences, line
    /// breaks and bidi overrides. The question shows one line per agent and
    /// role, emits no terminal control, and the confirmation sent back still
    /// names the original identities (PRD #1487 audit A3).
    #[test]
    fn a_hostile_remote_stop_set_cannot_steer_the_confirmation_terminal() {
        let hostile = hostile_stop_set();
        let port = FakePort::new(
            vec![
                Ok(Some(hello("0.1.0", "old"))),
                Ok(Some(hello("0.2.0", "new"))),
            ],
            vec![
                Ok(GatedQuery::Answered(
                    RestartDaemonReply::NeedsConfirmation {
                        at_stake: hostile.clone(),
                        stale: false,
                    },
                )),
                Ok(GatedQuery::Answered(RestartDaemonReply::Accepted {
                    from_version: "0.1.0".into(),
                    to_version: Some("0.2.0".into()),
                    successor: RestartSuccessor::Installed,
                    stopping: hostile.clone(),
                })),
            ],
        );
        let mut out = Vec::new();
        let (outcome, _) = run(
            &FakeInstaller::ok("0.2.0", InstallMethod::LocalBin),
            &port,
            &TtyDecider::new("r\n".as_bytes(), &mut out),
            &remote_plan(),
        );
        let asked = String::from_utf8(out).unwrap();
        assert_inert(&asked);
        // The genuine question and list are intact: a header, one agent line,
        // a header, one role line, then the question on the last line.
        let lines: Vec<&str> = asked.lines().collect();
        assert_eq!(lines.len(), 6, "{asked}");
        assert!(
            lines[0].starts_with("Restarting the daemon on 'box'"),
            "{asked}"
        );
        assert_eq!(lines[1], "  Agents:");
        assert!(lines[2].starts_with("    coder[2J[H]52;c;"), "{asked}");
        assert!(lines[2].contains("(pane p1Restart now?"), "{asked}");
        assert!(lines[2].ends_with("in /work/gnp.exe)"), "{asked}");
        assert_eq!(lines[3], "  Orchestration roles:");
        assert!(
            lines[4].starts_with("    team]8;;http://x: coder31m in pane p1"),
            "{asked}"
        );
        assert!(
            lines[5].starts_with("Restart now? [r] / Keep current daemon [K]:"),
            "{asked}"
        );

        // What went back to the daemon is the set as received, not the display
        // copy: the daemon compares identities.
        let requests = port.requests.borrow();
        assert_eq!(requests[1].confirm.as_ref(), Some(&hostile));

        let summary = outcome.summary("box");
        assert!(
            summary.starts_with("Restarted the daemon on 'box'"),
            "{summary}"
        );
        assert_inert(&summary);
    }

    /// Scenario: remote-reported versions and a refusal message carrying
    /// escape sequences and bidi overrides are shown inert in every summary
    /// arm, and a remote cannot make a summary arbitrarily long.
    #[test]
    fn remote_versions_and_messages_in_summaries_are_inert_and_bounded() {
        let evil_version = "0.1.0\u{1b}]0;pwned\u{7}\u{202e}";
        let evil_reason = format!("refused\u{1b}[31m\n\u{9b}2J{}", "x".repeat(100_000));
        let outcomes = [
            UpgradeOutcome::Restarted {
                from_version: evil_version.into(),
                to_version: evil_version.into(),
                stopped: hostile_stop_set(),
            },
            UpgradeOutcome::InstalledNotRestarted {
                from_version: Some(evil_version.into()),
                installed_version: evil_version.into(),
                reason: NotRestartedReason::KeptByUser {
                    at_stake: hostile_stop_set(),
                },
            },
            UpgradeOutcome::InstalledNotRestarted {
                from_version: Some(evil_version.into()),
                installed_version: evil_version.into(),
                reason: NotRestartedReason::InstalledBuildTooOld,
            },
            UpgradeOutcome::InstalledDaemonTooOld {
                installed_version: evil_version.into(),
                daemon_version: Some(evil_version.into()),
                remedy: "connect".into(),
            },
            UpgradeOutcome::Failed {
                stage: UpgradeStage::Restarting,
                reason: evil_reason.clone(),
                installed_version: Some(evil_version.into()),
                old_daemon_gone: false,
            },
        ];
        for outcome in outcomes {
            let summary = outcome.summary("box");
            assert_inert(&summary);
            assert!(summary.contains("0.1.0]0;pwned"), "{summary}");
            assert!(
                summary.len() < 16 * 1024,
                "unbounded: {} bytes",
                summary.len()
            );
        }
        // The failure reason keeps its own line break; the CSI residue is text.
        let failed = UpgradeOutcome::Failed {
            stage: UpgradeStage::Restarting,
            reason: evil_reason,
            installed_version: None,
            old_daemon_gone: false,
        }
        .summary("box");
        assert!(failed.contains("refused[31m\n2Jxxx"), "{failed}");
        assert!(failed.contains('…'), "a clamped reason is marked: {failed}");
    }

    #[test]
    fn summaries_say_what_happened_in_plain_words() {
        let restarted = UpgradeOutcome::Restarted {
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            stopped: live("a"),
        };
        let text = restarted.summary("box");
        assert!(text.contains("Restarted") && text.contains("0.1.0") && text.contains("0.2.0"));
        assert!(text.contains("pane-a"));

        for reason in [
            NotRestartedReason::KeptByUser {
                at_stake: live("a"),
            },
            NotRestartedReason::NoOneToAsk {
                at_stake: live("a"),
            },
            NotRestartedReason::StaleConfirmation {
                at_stake: live("a"),
            },
        ] {
            let text = UpgradeOutcome::InstalledNotRestarted {
                from_version: Some("0.1.0".into()),
                installed_version: "0.2.0".into(),
                reason,
            }
            .summary("box");
            assert!(text.starts_with("Installed 0.2.0 on 'box'"), "{text}");
            assert!(text.contains("not restarted") && text.contains("keeps running 0.1.0"));
            assert!(text.contains("pane-a") && text.contains("team"), "{text}");
            assert!(!text.contains("Restart now?"));
        }
        for reason in [
            NotRestartedReason::AnotherRestartInProgress,
            NotRestartedReason::NoDaemonRunning,
        ] {
            let text = UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: "0.2.0".into(),
                reason,
            }
            .summary("box");
            assert!(text.starts_with("Installed 0.2.0 on 'box'"), "{text}");
        }
        let failed = UpgradeOutcome::Failed {
            stage: UpgradeStage::Installing,
            reason: "download refused".into(),
            installed_version: None,
            old_daemon_gone: false,
        }
        .summary("box");
        assert!(failed.contains("failed while installing") && failed.contains("download refused"));
    }

    #[test]
    fn outcomes_serialize_with_kebab_case_tags() {
        let value = serde_json::to_value(UpgradeOutcome::InstalledNotRestarted {
            from_version: None,
            installed_version: "0.2.0".into(),
            reason: NotRestartedReason::NoOneToAsk {
                at_stake: RestartStopSet::default(),
            },
        })
        .unwrap();
        assert_eq!(value["outcome"], "installed-not-restarted");
        assert_eq!(value["reason"]["kind"], "no-one-to-ask");
        let value = serde_json::to_value(UpgradeOutcome::Failed {
            stage: UpgradeStage::Installing,
            reason: "x".into(),
            installed_version: None,
            old_daemon_gone: false,
        })
        .unwrap();
        assert_eq!(value["outcome"], "failed");
        assert_eq!(value["stage"], "installing");
        let value = serde_json::to_value(UpgradeOutcome::InstalledDaemonTooOld {
            installed_version: "0.2.0".into(),
            daemon_version: None,
            remedy: "r".into(),
        })
        .unwrap();
        assert_eq!(value["outcome"], "installed-daemon-too-old");
        for (reason, tag) in [
            (
                NotRestartedReason::AnotherRestartInProgress,
                "another-restart-in-progress",
            ),
            (NotRestartedReason::NoDaemonRunning, "no-daemon-running"),
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap()["kind"], tag);
        }
    }

    #[test]
    fn upgrade_offer_follows_the_d8_table() {
        assert_eq!(
            upgrade_offer_against("0.46.0", Some("0.45.1")),
            UpgradeOffer::Offered {
                from: "0.45.1".into(),
                to: "0.46.0".into()
            }
        );
        assert_eq!(
            upgrade_offer_against("0.46.0", Some("0.46.0")),
            UpgradeOffer::Current
        );
        // Same release, different build stamps.
        assert_eq!(
            upgrade_offer_against("0.46.0", Some("0.46.0-g5a56361")),
            UpgradeOffer::Current
        );
        assert_eq!(
            upgrade_offer_against("0.46.0-gabc1234-dirty", Some("v0.46.0")),
            UpgradeOffer::Current
        );
        assert_eq!(
            upgrade_offer_against("0.46.0", Some("0.47.0")),
            UpgradeOffer::DaemonNewer {
                daemon: "0.47.0".into()
            }
        );
        assert_eq!(upgrade_offer_against("0.46.0", None), UpgradeOffer::Unknown);
        assert_eq!(
            upgrade_offer_against("0.46.0", Some("garbage")),
            UpgradeOffer::Unknown
        );
        // A real pre-release is not a build stamp.
        assert!(matches!(
            upgrade_offer_against("0.46.0", Some("0.46.0-alpha.1")),
            UpgradeOffer::Offered { .. }
        ));
        assert_eq!(
            serde_json::to_value(UpgradeOffer::Current).unwrap()["kind"],
            "current"
        );
        assert_eq!(
            serde_json::to_value(UpgradeOffer::DaemonNewer { daemon: "1".into() }).unwrap()["kind"],
            "daemon-newer"
        );
        assert!(matches!(
            upgrade_offer(Some("0.0.1")),
            UpgradeOffer::Offered { .. } | UpgradeOffer::Unknown
        ));
    }

    #[test]
    fn client_is_newer_is_newer_only() {
        assert!(client_is_newer("0.31.1", "0.31.0"));
        assert!(!client_is_newer("0.31.0", "0.31.0"));
        assert!(!client_is_newer("0.31.0", "0.31.1"));
        assert!(client_is_newer("v0.32.0", "0.31.9"));
        assert!(!client_is_newer("garbage", "0.31.0"));
        assert!(!client_is_newer("0.31.0", "garbage"));
        assert!(!client_is_newer("0.31.0-gabcdef0", "0.31.0"));
    }
}
