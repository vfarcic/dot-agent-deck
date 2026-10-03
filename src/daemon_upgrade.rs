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
    DaemonClient, Endpoint, GatedQuery, LocalEndpoint, RestartDaemonRequest,
};
use crate::daemon_protocol::{
    AttachResponse, RestartDaemonReply, RestartRefusalReason, RestartStopSet, RestartSuccessor,
};
use crate::remote::{SshExecutor, SystemSshExecutor};
use crate::remote_daemon::{RemoteDaemonError, SshDaemonPort};

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
    Failed {
        stage: UpgradeStage,
        reason: String,
        installed_version: Option<String>,
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

impl UpgradeOutcome {
    /// Plain-language rendering for a person; the CLI prints it. Clients with
    /// their own surface may build their own from the fields instead.
    pub fn summary(&self, deck: &str) -> String {
        match self {
            Self::Restarted {
                from_version,
                to_version,
                stopped,
            } => {
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
                let running = from_version
                    .as_deref()
                    .map(|v| format!(" It keeps running {v}."))
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
                }
            }
            Self::InstalledDaemonTooOld {
                installed_version,
                daemon_version,
                remedy,
            } => {
                let daemon = daemon_version
                    .as_deref()
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default();
                format!(
                    "Installed {installed_version} on '{deck}', but the running daemon{daemon} is too old to restart itself, so it keeps running. {remedy}"
                )
            }
            Self::Failed {
                stage,
                reason,
                installed_version,
            } => {
                let doing = match stage {
                    UpgradeStage::Installing => "installing the new build",
                    UpgradeStage::Restarting => "restarting the daemon",
                    UpgradeStage::Verifying => "checking the restarted daemon",
                };
                let mut text = format!("Upgrade of '{deck}' failed while {doing}: {reason}");
                match (stage, installed_version) {
                    (UpgradeStage::Verifying, Some(v)) => text.push_str(&format!(
                        "\n{v} is installed; the daemon was asked to restart onto it, and the next one to start runs it."
                    )),
                    (_, Some(v)) => text.push_str(&format!(
                        "\n{v} is installed; the daemon that was running keeps running."
                    )),
                    (_, None) => {
                        text.push_str("\nNothing was changed; the daemon that was running keeps running.")
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
pub fn describe_stop_set(set: &RestartStopSet) -> String {
    let mut text = String::new();
    if !set.agents.is_empty() {
        text.push_str("  Agents:\n");
        for agent in &set.agents {
            let mut place = Vec::new();
            if let Some(pane) = &agent.pane_id {
                place.push(format!("pane {pane}"));
            }
            if let Some(cwd) = &agent.cwd {
                place.push(format!("in {cwd}"));
            }
            if place.is_empty() {
                text.push_str(&format!("    {}\n", agent.label));
            } else {
                text.push_str(&format!("    {} ({})\n", agent.label, place.join(", ")));
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
                role.orchestration, role.role, role.pane_id
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

/// Puts a build in place. `Err` is a plain-language reason.
pub trait Installer {
    fn install(&self, version: &str) -> Result<InstalledBuild, String>;
}

/// Reaches the running daemon.
pub trait DaemonPort {
    /// The running daemon's `Hello`, or `Ok(None)` when none is running. Never
    /// starts one.
    fn probe(&self) -> Result<Option<AttachResponse>, String>;
    /// Send the restart request. Must go through
    /// [`DaemonClient::restart_daemon`], which withholds it from a daemon that
    /// does not advertise it — `Unsupported` then means the daemon is too old.
    fn restart(&self, req: &RestartDaemonRequest)
    -> Result<GatedQuery<RestartDaemonReply>, String>;
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
    fn legacy_restart(&self, _decider: &dyn RestartDecider) -> Option<UpgradeOutcome> {
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
/// When the new build is the same build as the old (a reinstall), its answer
/// cannot be told from the old daemon's. If the endpoint was never seen empty,
/// a matching answer is accepted once it has held this long — longer than the
/// old daemon's whole drain (3s) plus margin, so it is not the old daemon.
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
        Err(reason) => {
            return UpgradeOutcome::Failed {
                stage: UpgradeStage::Installing,
                reason,
                installed_version: None,
            };
        }
    };
    daemon.installed(&installed);
    let restarting_failed = |reason: String| UpgradeOutcome::Failed {
        stage: UpgradeStage::Restarting,
        reason,
        installed_version: Some(installed.version.clone()),
    };

    progress(UpgradeProgress {
        stage: UpgradeStage::Restarting,
        detail: None,
    });
    let hello = match daemon.probe() {
        Ok(Some(hello)) => hello,
        Ok(None) => {
            return UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: installed.version,
                reason: NotRestartedReason::NoDaemonRunning,
            };
        }
        Err(reason) => return restarting_failed(reason),
    };
    let from_version = hello.daemon_version.clone();
    let from_build = hello.build_version.clone();
    let not_restarted = |reason: NotRestartedReason| UpgradeOutcome::InstalledNotRestarted {
        from_version: from_version.clone(),
        installed_version: installed.version.clone(),
        reason,
    };

    let mut confirm: Option<RestartStopSet> = None;
    let mut stopped = None;
    for round in 0..MAX_CONFIRM_ROUNDS {
        let request = RestartDaemonRequest {
            confirm: confirm.clone(),
            expected_version: Some(installed.version.clone()),
            successor: plan.successor,
        };
        match daemon.restart(&request) {
            Err(reason) => return restarting_failed(reason),
            Ok(GatedQuery::Unsupported) => {
                return daemon.legacy_restart(decider).unwrap_or_else(|| {
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
        if let Err(reason) = daemon.spawn_successor() {
            return restarting_failed(reason);
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
    match wait_for_successor(
        daemon,
        &expected,
        from_build.as_deref(),
        endpoint_emptied,
        timing,
    ) {
        Ok(()) => UpgradeOutcome::Restarted {
            from_version: from_version.unwrap_or_else(|| "an unknown version".into()),
            to_version: installed.version,
            stopped,
        },
        Err(reason) => UpgradeOutcome::Failed {
            stage: UpgradeStage::Verifying,
            reason,
            installed_version: Some(installed.version),
        },
    }
}

/// The remedy for a daemon too old to be asked to restart.
fn too_old_remedy(deck: &str) -> String {
    format!(
        "Its agents keep running on the old build. To switch, connect with `dot-agent-deck connect {deck}` and accept its restart prompt, or run `dot-agent-deck daemon restart` on that machine."
    )
}

/// What the restarted daemon must report.
enum Expect {
    /// `daemon_version` equal to the installed version.
    Version(String),
    /// `build_version` equal to the client's own build.
    Build(String),
}

/// Poll `daemon` until the successor answers as `expected`.
///
/// An answer from the OLD daemon must not count: it keeps answering while it
/// drains. A matching answer is the successor's when the endpoint was seen
/// empty in between, or its build differs from the old daemon's, or (a
/// reinstall of the very same build) it has kept matching for `settle`.
fn wait_for_successor(
    daemon: &dyn DaemonPort,
    expected: &Expect,
    from_build: Option<&str>,
    mut endpoint_emptied: bool,
    timing: Timing,
) -> Result<(), String> {
    let deadline = Instant::now() + timing.timeout;
    let mut matching_since: Option<Instant> = None;
    let mut last_seen: Option<String> = None;
    loop {
        match daemon.probe() {
            Ok(None) => {
                endpoint_emptied = true;
                matching_since = None;
            }
            Ok(Some(hello)) => {
                let matches = match expected {
                    Expect::Version(v) => hello.daemon_version.as_deref() == Some(v.as_str()),
                    Expect::Build(b) => hello.build_version.as_deref() == Some(b.as_str()),
                };
                last_seen = hello.daemon_version.clone().or(hello.build_version.clone());
                if matches {
                    let replaced = endpoint_emptied || hello.build_version.as_deref() != from_build;
                    if replaced {
                        return Ok(());
                    }
                    let since = *matching_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= timing.settle {
                        return Ok(());
                    }
                } else {
                    matching_since = None;
                }
            }
            // A probe that learned nothing is neither an answer nor a gap.
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            let seen = last_seen
                .map(|v| format!(" (the daemon answering reports {v})"))
                .unwrap_or_default();
            return Err(format!(
                "restarted, but the new daemon did not answer within {}s{seen}",
                timing.timeout.as_secs()
            ));
        }
        std::thread::sleep(timing.poll);
    }
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

/// Installs on a registered remote through [`crate::remote::upgrade_reporting_to`]
/// — the download to `~/.local/bin`, or `brew upgrade` where Homebrew owns the
/// install — refreshing the hooks and the deck list's row. What that prints
/// goes to `out`.
pub struct SshInstaller<E: SshExecutor = SystemSshExecutor> {
    pub name: String,
    pub remotes_path: PathBuf,
    pub executor: E,
    /// `remote upgrade --no-install`: only verify what is already there.
    pub no_install: bool,
    pub release_base: String,
    pub out: RefCell<Box<dyn Write>>,
}

impl SshInstaller<SystemSshExecutor> {
    /// The production installer for remote `name`, reporting to `out`.
    pub fn new(name: &str, remotes_path: PathBuf, out: Box<dyn Write>) -> Self {
        Self {
            name: name.to_string(),
            remotes_path,
            executor: upgrade_ssh_executor(),
            no_install: false,
            release_base: crate::remote::RELEASE_BASE.to_string(),
            out: RefCell::new(out),
        }
    }
}

impl<E: SshExecutor> Installer for SshInstaller<E> {
    fn install(&self, version: &str) -> Result<InstalledBuild, String> {
        let opts = crate::remote::UpgradeOptions {
            name: self.name.clone(),
            version: version.to_string(),
            no_install: self.no_install,
            release_base: self.release_base.clone(),
        };
        let mut out = self.out.borrow_mut();
        let entry = crate::remote::upgrade_reporting_to(
            &opts,
            &self.executor,
            &self.remotes_path,
            &mut *out,
        )
        .map_err(|e| e.to_string())?;
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
    fn install(&self, _version: &str) -> Result<InstalledBuild, String> {
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

/// A remote daemon, reached through the remote's freshly installed binary.
impl<E: SshExecutor> DaemonPort for SshDaemonPort<E> {
    fn probe(&self) -> Result<Option<AttachResponse>, String> {
        match SshDaemonPort::probe(self) {
            Ok(probe) if probe.running => Ok(probe.hello),
            Ok(_) => Ok(None),
            Err(RemoteDaemonError::Unsupported { .. }) => Err(format!(
                "the installed build at {} is too old to report on the running daemon",
                self.binary()
            )),
            Err(e) => Err(e.to_string()),
        }
    }

    fn restart(
        &self,
        req: &RestartDaemonRequest,
    ) -> Result<GatedQuery<RestartDaemonReply>, String> {
        match self.restart_installed(req.expected_version.as_deref(), req.confirm.as_ref()) {
            Ok(report) if report.unsupported => Ok(GatedQuery::Unsupported),
            Ok(report) => match report.reply {
                Some(reply) => Ok(GatedQuery::Answered(reply)),
                // The daemon went away between the probe and the request.
                None if !report.running => {
                    Err("the daemon stopped before it could be asked to restart".into())
                }
                None => Err("the remote reported no answer from the daemon".into()),
            },
            // An installed build too old to drive the restart (Homebrew can
            // land an older tap release): the daemon cannot be asked.
            Err(RemoteDaemonError::Unsupported { .. } | RemoteDaemonError::Malformed(_)) => {
                Ok(GatedQuery::Unsupported)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn installed(&self, build: &InstalledBuild) {
        self.set_binary(build.binary.clone());
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
    fn release_then_spawn(&self) -> Result<(), String> {
        let deadline = Instant::now() + LOCAL_RELEASE_TIMEOUT;
        while DaemonPort::probe(self)?.is_some() {
            if Instant::now() >= deadline {
                return Err(format!(
                    "the old daemon was still running {}s after it agreed to stop",
                    LOCAL_RELEASE_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(VERIFY_POLL);
        }
        (self.spawn)()
    }
}

impl DaemonPort for WireDaemonPort {
    fn probe(&self) -> Result<Option<AttachResponse>, String> {
        self.handle
            .block_on(async {
                tokio::time::timeout(LOCAL_PROBE_TIMEOUT, self.client.probe_running()).await
            })
            .map_err(|_| {
                format!(
                    "no answer from the daemon within {}s",
                    LOCAL_PROBE_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| e.to_string())
    }

    fn restart(
        &self,
        req: &RestartDaemonRequest,
    ) -> Result<GatedQuery<RestartDaemonReply>, String> {
        self.handle
            .block_on(self.client.restart_daemon(req.clone()))
            .map_err(|e| e.to_string())
    }

    fn spawn_successor(&self) -> Result<(), String> {
        self.release_then_spawn()
    }

    /// A local daemon without the restart request: the existing stop path,
    /// after asking `decider` about the same agents and roles the request
    /// would have named. Only for a local deck; a remote one gets `None`.
    fn legacy_restart(&self, decider: &dyn RestartDecider) -> Option<UpgradeOutcome> {
        let local = self.local.clone()?;
        Some(self.legacy_restart_local(&local, decider))
    }
}

impl WireDaemonPort {
    fn legacy_restart_local(
        &self,
        local: &LocalEndpoint,
        decider: &dyn RestartDecider,
    ) -> UpgradeOutcome {
        use crate::daemon_stop::{StopError, StopOutcome, run_daemon_stop};
        let version = CLIENT_VERSION.to_string();
        let failed = |stage, reason: String| UpgradeOutcome::Failed {
            stage,
            reason,
            installed_version: Some(version.clone()),
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
            Err(reason) => return failed(UpgradeStage::Restarting, reason),
        };
        let from_version = hello.daemon_version.clone();
        // The unforced stop refuses while anything is live — the same refusal
        // `daemon stop` gives — and stops an idle daemon outright.
        let mut stopped = RestartStopSet::default();
        let first = self.handle.block_on(run_daemon_stop(local, false));
        let stop = match first {
            Err(StopError::LiveAgents { .. } | StopError::LiveOrchestrations { .. }) => {
                let agents = match self.handle.block_on(self.client.list_agents()) {
                    Ok(agents) => agents,
                    Err(e) => return failed(UpgradeStage::Restarting, e.to_string()),
                };
                let roles = match first {
                    Err(StopError::LiveOrchestrations { roles }) => roles,
                    _ => Vec::new(),
                };
                let at_stake = crate::daemon_restart::stop_set(&roles, &agents);
                let not_restarted = |reason| UpgradeOutcome::InstalledNotRestarted {
                    from_version: from_version.clone(),
                    installed_version: version.clone(),
                    reason,
                };
                match decider.decide("local", &at_stake, false) {
                    RestartChoice::RestartNow => {}
                    RestartChoice::KeepCurrent => {
                        return not_restarted(NotRestartedReason::KeptByUser { at_stake });
                    }
                    RestartChoice::NoOneToAsk => {
                        return not_restarted(NotRestartedReason::NoOneToAsk { at_stake });
                    }
                }
                stopped = at_stake;
                self.handle.block_on(run_daemon_stop(local, true))
            }
            other => other,
        };
        match stop {
            Ok(StopOutcome::NoDaemonRunning | StopOutcome::Stopped { .. })
            | Ok(StopOutcome::ForceKilled { .. }) => {}
            Err(e) => return failed(UpgradeStage::Restarting, e.to_string()),
        }
        if let Err(reason) = self.release_then_spawn() {
            return failed(UpgradeStage::Restarting, reason);
        }
        let expected = Expect::Build(crate::build_id::local_build_id());
        match wait_for_successor(self, &expected, None, true, PRODUCTION_TIMING) {
            Ok(()) => UpgradeOutcome::Restarted {
                from_version: from_version.unwrap_or_else(|| "an unknown version".into()),
                to_version: version,
                stopped,
            },
            Err(reason) => failed(UpgradeStage::Verifying, reason),
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
        result: Result<InstalledBuild, String>,
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
                result: Err(reason.into()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Installer for FakeInstaller {
        fn install(&self, version: &str) -> Result<InstalledBuild, String> {
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

    type Probe = Result<Option<AttachResponse>, String>;
    type Restart = Result<GatedQuery<RestartDaemonReply>, String>;

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
        fn legacy_restart(&self, _decider: &dyn RestartDecider) -> Option<UpgradeOutcome> {
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
            }
        );
        assert!(outcome.is_failure());
        assert_eq!(stages, [UpgradeStage::Installing]);
        assert!(port.requests.borrow().is_empty());
        assert!(port.installed.borrow().is_none());
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
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(*stage, UpgradeStage::Verifying);
        assert!(reason.starts_with("restarted, but the new daemon did not answer"));
        assert_eq!(installed_version.as_deref(), Some("0.2.0"));
        assert_eq!(stages.last(), Some(&UpgradeStage::Verifying));
    }

    #[test]
    fn the_old_daemon_answering_the_same_build_is_not_the_successor_until_settled() {
        // A reinstall of the same build: the endpoint is never seen empty and
        // the build does not change, so the answer only counts once it has held
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
