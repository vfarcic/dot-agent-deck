//! Upgrade every remote deck's daemon in one go (issue #1600, the follow-up
//! PRD #1487 D9 deferred).
//!
//! The whole batch policy lives here, and the CLI (`remote upgrade --all`),
//! the TUI's Upgrade all daemons dialog and the desktop app's Upgrade all
//! dialog all call it, so the three cannot disagree about which deck is
//! upgraded, what stops the batch, or what a deck's result is called
//! (CLAUDE.md rules 18 and 22). Each deck's upgrade is the unchanged
//! [`crate::daemon_upgrade::upgrade_daemon`].
//!
//! 1. [`check_all`] checks every deck at the same time and decides, through
//!    the same [`upgrade_offer_against`] a single Upgrade uses (PRD #1487 D8),
//!    whether it is upgraded, skipped (and why), or could not be checked.
//! 2. [`run_plan`] upgrades the decks the check picked **one after another**:
//!    the live-agent question is interactive, so two daemons must not ask at
//!    once. Each daemon with live agents or roles gets its own question, naming
//!    its own; there is no answer for all of them. A failure on one deck does
//!    not stop the batch. Only the caller's `stop` does, and only between
//!    decks: the deck being upgraded finishes.
//! 3. [`BatchReport`] holds one [`DeckReport`] per deck, in deck-list order,
//!    each with its [`DeckStatus`] and the line a person reads.
//!
//! The local daemon is not in the batch: the TUI and the desktop app upgrade
//! it on their own when they start, and `remote upgrade` never covered it.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::daemon_protocol::{AttachResponse, RestartStopSet, RestartSuccessor};
use crate::daemon_upgrade::{
    CLIENT_VERSION, DaemonPort, NotRestartedReason, PortError, RestartDecider, SshInstaller,
    UpgradeOffer, UpgradeOutcome, UpgradePlan, UpgradeProgress, UpgradeStage, upgrade_daemon,
    upgrade_offer_against, upgrade_ssh_executor,
};
use crate::remote::RemoteEntry;
use crate::remote_daemon::SshDaemonPort;
use crate::untrusted_text::{
    REMOTE_MESSAGE_MAX_BYTES, REMOTE_NAME_MAX_BYTES, display_line, display_message,
};

// ---------------------------------------------------------------------------
// What a check finds
// ---------------------------------------------------------------------------

/// Why a deck is not upgraded although it was checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SkipReason {
    /// Its daemon runs the same release as this client.
    Current { version: String },
    /// Its daemon runs a newer release than this client.
    DaemonNewer { daemon: String },
    /// No daemon runs there, so there is no daemon to upgrade.
    NoDaemonRunning,
    /// Its daemon's version could not be read, or one of the two builds does
    /// not know its own version, so no upgrade is offered (PRD #1487 D8).
    VersionUnknown,
}

/// What checking one deck found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DeckCheck {
    /// Its daemon is older than this client: upgrade it from `from` to `to`.
    Upgrade { from: String, to: String },
    /// Left alone, for `reason`.
    Skip { reason: SkipReason },
    /// The deck could not be reached or asked, so it was not checked.
    Unreachable { reason: String },
}

impl DeckCheck {
    /// The check for a daemon reporting `version`, against `client`.
    fn from_version(client: &str, version: Option<&str>) -> Self {
        match upgrade_offer_against(client, version) {
            UpgradeOffer::Offered { from, to } => Self::Upgrade { from, to },
            UpgradeOffer::Current => Self::Skip {
                reason: SkipReason::Current {
                    version: version.unwrap_or_default().to_string(),
                },
            },
            UpgradeOffer::DaemonNewer { daemon } => Self::Skip {
                reason: SkipReason::DaemonNewer { daemon },
            },
            UpgradeOffer::Unknown => Self::Skip {
                reason: SkipReason::VersionUnknown,
            },
        }
    }
}

/// Classify what probing a deck's daemon returned, against `client`.
///
/// - A daemon answered: its reported version decides. A daemon whose `Hello`
///   names no version predates every release that reports one (PRD #161), so
///   it is older than any client that knows its own version and is upgraded;
///   the installed binary's version says nothing about it, since an earlier
///   upgrade may have installed a newer binary and kept this daemon running.
/// - No daemon is running: skipped.
/// - The installed binary predates `daemon probe`: it is older than this
///   client's every release, so the binary's own `--version` decides.
/// - Anything else: the deck could not be checked.
pub fn check_from_probe(
    client: &str,
    probe: Result<Option<AttachResponse>, PortError>,
    binary_version: &dyn Fn() -> Result<String, String>,
) -> DeckCheck {
    let by_binary = || match binary_version() {
        Ok(version) => DeckCheck::from_version(client, Some(&version)),
        Err(reason) => DeckCheck::Unreachable { reason },
    };
    match probe {
        Ok(Some(hello)) => match hello.daemon_version.as_deref() {
            Some(version) => DeckCheck::from_version(client, Some(version)),
            None if crate::version::is_unknown_version(client) => DeckCheck::Skip {
                reason: SkipReason::VersionUnknown,
            },
            None => DeckCheck::Upgrade {
                from: hello
                    .build_version
                    .clone()
                    .unwrap_or_else(|| "an older release".to_string()),
                to: client.to_string(),
            },
        },
        Ok(None) => DeckCheck::Skip {
            reason: SkipReason::NoDaemonRunning,
        },
        Err(PortError::InstalledBuildTooOld(_)) => by_binary(),
        Err(PortError::NoDaemonRunning) => DeckCheck::Skip {
            reason: SkipReason::NoDaemonRunning,
        },
        Err(error) => DeckCheck::Unreachable {
            reason: error.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// The seam: one deck
// ---------------------------------------------------------------------------

/// One deck the batch can check and upgrade.
///
/// `Sync` because [`check_all`] checks every deck at once, each on its own
/// thread.
pub trait BatchDeck: Sync {
    /// The deck's name, as the deck list holds it.
    fn name(&self) -> &str;
    /// Whether to upgrade it. Never changes anything on the deck.
    fn check(&self) -> DeckCheck;
    /// Upgrade it with [`upgrade_daemon`], asking `decider` if its daemon
    /// names live work.
    fn upgrade(
        &self,
        decider: &dyn RestartDecider,
        progress: &mut dyn FnMut(UpgradeProgress),
    ) -> UpgradeOutcome;
}

/// A remote deck on the deck list, reached over SSH exactly as a single
/// `remote upgrade` reaches it: the row's SSH route, recorded binary and
/// daemon socket ([`SshInstaller::for_entry`], [`SshDaemonPort::for_entry`]).
pub struct RemoteBatchDeck {
    entry: RemoteEntry,
    remotes_path: PathBuf,
    /// Where the installer's step-by-step lines go: a terminal for the CLI,
    /// nowhere for a client that shows the stages itself.
    report: fn() -> Box<dyn Write>,
}

impl RemoteBatchDeck {
    pub fn new(entry: RemoteEntry, remotes_path: PathBuf, report: fn() -> Box<dyn Write>) -> Self {
        Self {
            entry,
            remotes_path,
            report,
        }
    }

    /// The deck-list row this deck was read from.
    pub fn entry(&self) -> &RemoteEntry {
        &self.entry
    }

    /// One deck per row of the deck list at `remotes_path`, in its order.
    pub fn from_deck_list(
        remotes_path: &std::path::Path,
        report: fn() -> Box<dyn Write>,
    ) -> Result<Vec<Self>, String> {
        let file = crate::remote::RemotesFile::load(remotes_path).map_err(|e| e.to_string())?;
        Ok(file
            .remotes
            .into_iter()
            .map(|entry| Self::new(entry, remotes_path.to_path_buf(), report))
            .collect())
    }
}

/// [`std::io::sink`], for a client that shows the stages itself.
pub fn no_report() -> Box<dyn Write> {
    Box::new(std::io::sink())
}

impl BatchDeck for RemoteBatchDeck {
    fn name(&self) -> &str {
        &self.entry.name
    }

    fn check(&self) -> DeckCheck {
        let port = SshDaemonPort::for_entry(upgrade_ssh_executor(), &self.entry);
        let probe = <SshDaemonPort<_> as DaemonPort>::probe(&port);
        check_from_probe(CLIENT_VERSION, probe, &|| {
            port.version().map_err(|failure| version_failure(&failure))
        })
    }

    fn upgrade(
        &self,
        decider: &dyn RestartDecider,
        progress: &mut dyn FnMut(UpgradeProgress),
    ) -> UpgradeOutcome {
        let installer =
            SshInstaller::for_entry(&self.entry, self.remotes_path.clone(), (self.report)());
        let port = SshDaemonPort::for_entry(upgrade_ssh_executor(), &self.entry);
        let plan = UpgradePlan {
            version: CLIENT_VERSION.to_string(),
            successor: RestartSuccessor::Installed,
        };
        upgrade_daemon(
            &self.entry.name,
            &plan,
            &installer,
            &port,
            decider,
            progress,
        )
    }
}

/// Why `<binary> --version` on a remote gave no version, in plain words.
fn version_failure(failure: &crate::connect::VersionProbeFailure) -> String {
    use crate::connect::VersionProbeFailure;
    match failure {
        VersionProbeFailure::Ssh(error) => error.to_string(),
        VersionProbeFailure::Truncated => {
            "its dot-agent-deck printed far more than a version check prints".to_string()
        }
        VersionProbeFailure::BinaryMissing => "dot-agent-deck is not installed there".to_string(),
        VersionProbeFailure::Failed { detail } => {
            format!("its dot-agent-deck failed to run: {detail}")
        }
    }
}

// ---------------------------------------------------------------------------
// The plan and the report
// ---------------------------------------------------------------------------

/// One deck and what its check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDeck {
    pub deck: String,
    pub check: DeckCheck,
}

impl PlannedDeck {
    /// What the plan says about this deck, in plain words.
    pub fn line(&self) -> String {
        match &self.check {
            DeckCheck::Upgrade { from, to } => {
                format!("Will be upgraded from {} to {}.", shown(from), shown(to))
            }
            DeckCheck::Skip { reason } => skip_line(reason),
            DeckCheck::Unreachable { reason } => unreachable_line(reason),
        }
    }
}

/// What [`check_all`] found, one entry per deck in deck-list order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchPlan {
    pub decks: Vec<PlannedDeck>,
}

impl BatchPlan {
    /// How many decks the run would upgrade.
    pub fn to_upgrade(&self) -> usize {
        self.decks
            .iter()
            .filter(|planned| matches!(planned.check, DeckCheck::Upgrade { .. }))
            .count()
    }

    /// The plan as a person reads it: a headline, then one line per deck.
    pub fn summary(&self) -> String {
        if self.decks.is_empty() {
            return NO_REMOTE_DECKS.to_string();
        }
        let mut text = format!(
            "Checked {}: {} to upgrade.",
            count(self.decks.len(), "remote deck", "remote decks"),
            self.to_upgrade()
        );
        for planned in &self.decks {
            text.push_str(&format!("\n  {}: {}", shown(&planned.deck), planned.line()));
        }
        text
    }
}

/// What a batch says when the deck list holds no remote deck.
pub const NO_REMOTE_DECKS: &str =
    "No remote decks are on the deck list, so there is nothing to upgrade.";

/// How one deck's part of the batch ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DeckResult {
    /// [`upgrade_daemon`] ran, and this is what it did.
    Ran { outcome: UpgradeOutcome },
    /// The check left it alone.
    Skipped { reason: SkipReason },
    /// The check could not reach it.
    Unreachable { reason: String },
    /// The user stopped the batch before it reached this deck.
    NotStarted,
}

/// The four ways a deck's line reads, for the colour or icon beside it and for
/// the counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeckStatus {
    /// Its daemon now runs the new build.
    Upgraded,
    /// The new build is installed and the daemon that was running keeps
    /// running.
    Kept,
    /// Left alone: the check said so, or the user stopped the batch first.
    Skipped,
    /// Its upgrade failed, or it could not be checked.
    Failed,
}

impl DeckResult {
    pub fn status(&self) -> DeckStatus {
        match self {
            Self::Ran {
                outcome: UpgradeOutcome::Restarted { .. },
            } => DeckStatus::Upgraded,
            Self::Ran {
                outcome:
                    UpgradeOutcome::InstalledNotRestarted { .. }
                    | UpgradeOutcome::InstalledDaemonTooOld { .. },
            } => DeckStatus::Kept,
            Self::Ran {
                outcome: UpgradeOutcome::Failed { .. },
            }
            | Self::Unreachable { .. } => DeckStatus::Failed,
            Self::Skipped { .. } | Self::NotStarted => DeckStatus::Skipped,
        }
    }

    /// The deck's result in one line of plain words (CLAUDE.md rule 21).
    /// Every remote-reported value goes through [`crate::untrusted_text`].
    pub fn line(&self) -> String {
        match self {
            Self::Ran { outcome } => outcome_line(outcome),
            Self::Skipped { reason } => skip_line(reason),
            Self::Unreachable { reason } => unreachable_line(reason),
            Self::NotStarted => {
                "Not started: you stopped the upgrade before it reached this deck.".to_string()
            }
        }
    }
}

/// One deck's part of the batch, with the words every client shows for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckReport {
    pub deck: String,
    pub status: DeckStatus,
    /// [`DeckResult::line`], computed once so every client shows these words.
    pub line: String,
    pub result: DeckResult,
}

impl DeckReport {
    pub fn new(deck: &str, result: DeckResult) -> Self {
        Self {
            deck: deck.to_string(),
            status: result.status(),
            line: result.line(),
            result,
        }
    }
}

/// The batch's result: one [`DeckReport`] per deck, in deck-list order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReport {
    pub decks: Vec<DeckReport>,
}

impl BatchReport {
    /// Whether the CLI exits non-zero: any deck failed, including one that
    /// could not be checked. Skipped decks and kept daemons are not failures,
    /// as they are not for a single upgrade.
    pub fn is_failure(&self) -> bool {
        self.decks
            .iter()
            .any(|deck| deck.status == DeckStatus::Failed)
    }

    /// How many decks ended with `status`.
    pub fn count(&self, status: DeckStatus) -> usize {
        self.decks
            .iter()
            .filter(|deck| deck.status == status)
            .count()
    }

    /// The headline: how many of each, leaving out the kinds with none.
    pub fn headline(&self) -> String {
        if self.decks.is_empty() {
            return NO_REMOTE_DECKS.to_string();
        }
        let parts: Vec<String> = [
            (DeckStatus::Upgraded, "upgraded"),
            (DeckStatus::Kept, "kept running"),
            (DeckStatus::Skipped, "skipped"),
            (DeckStatus::Failed, "failed"),
        ]
        .into_iter()
        .filter_map(|(status, word)| {
            let n = self.count(status);
            (n > 0).then(|| format!("{n} {word}"))
        })
        .collect();
        format!(
            "Upgrade all: {} of {}.",
            parts.join(", "),
            count(self.decks.len(), "remote deck", "remote decks")
        )
    }

    /// The headline, then one line per deck: what the CLI prints at the end.
    pub fn summary(&self) -> String {
        let mut text = self.headline();
        for deck in &self.decks {
            text.push_str(&format!("\n  {}: {}", shown(&deck.deck), deck.line));
        }
        text
    }
}

// ---------------------------------------------------------------------------
// Running the batch
// ---------------------------------------------------------------------------

/// What [`run_plan`] reports while it runs, in order: for each deck it
/// upgrades, `Started`, its `Progress`, then `Finished`; and `Finished` alone
/// for a deck it does not upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum BatchEvent {
    /// The upgrade of `deck`, the `index`th in the plan, has started.
    Started { index: usize, deck: String },
    /// A stage of that upgrade.
    Progress {
        index: usize,
        deck: String,
        progress: UpgradeProgress,
    },
    /// The deck's part is over.
    Finished { index: usize, report: DeckReport },
}

/// How many decks [`check_all`] checks at once. Each check is a thread and an
/// ssh process, so a long deck list is checked this many at a time rather
/// than all at once.
pub const MAX_PARALLEL_CHECKS: usize = 16;

/// Check every deck, up to [`MAX_PARALLEL_CHECKS`] at the same time, and
/// return what each check found in the order of `decks`.
pub fn check_all(decks: &[&dyn BatchDeck]) -> BatchPlan {
    let mut checks: Vec<DeckCheck> = Vec::with_capacity(decks.len());
    for chunk in decks.chunks(MAX_PARALLEL_CHECKS) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|deck| scope.spawn(move || deck.check()))
                .collect();
            checks.extend(handles.into_iter().map(|handle| {
                handle.join().unwrap_or_else(|_| DeckCheck::Unreachable {
                    reason: "checking it stopped unexpectedly".to_string(),
                })
            }));
        });
    }
    BatchPlan {
        decks: decks
            .iter()
            .zip(checks)
            .map(|(deck, check)| PlannedDeck {
                deck: deck.name().to_string(),
                check,
            })
            .collect(),
    }
}

/// Upgrade the decks `plan` picked, one after another, in its order.
///
/// `decks` must be the slice `plan` was checked from. Each picked deck runs
/// [`BatchDeck::upgrade`] with `decider`, which is asked only by a daemon that
/// names live work, and only about that daemon's own. A deck that fails is
/// reported and the next one starts. `stop` is asked before each deck starts;
/// once it says yes, every deck not yet started is [`DeckResult::NotStarted`].
pub fn run_plan(
    decks: &[&dyn BatchDeck],
    plan: &BatchPlan,
    decider: &dyn RestartDecider,
    stop: &dyn Fn() -> bool,
    events: &mut dyn FnMut(BatchEvent),
) -> BatchReport {
    assert_eq!(
        decks.len(),
        plan.decks.len(),
        "run_plan needs the decks the plan was checked from"
    );
    let mut reports = Vec::with_capacity(plan.decks.len());
    let mut stopped = false;
    for (index, (deck, planned)) in decks.iter().zip(&plan.decks).enumerate() {
        let result = match &planned.check {
            DeckCheck::Skip { reason } => DeckResult::Skipped {
                reason: reason.clone(),
            },
            DeckCheck::Unreachable { reason } => DeckResult::Unreachable {
                reason: reason.clone(),
            },
            DeckCheck::Upgrade { .. } => {
                stopped = stopped || stop();
                if stopped {
                    DeckResult::NotStarted
                } else {
                    let name = deck.name().to_string();
                    events(BatchEvent::Started {
                        index,
                        deck: name.clone(),
                    });
                    let outcome = deck.upgrade(decider, &mut |progress| {
                        events(BatchEvent::Progress {
                            index,
                            deck: name.clone(),
                            progress,
                        })
                    });
                    DeckResult::Ran { outcome }
                }
            }
        };
        let report = DeckReport::new(&planned.deck, result);
        events(BatchEvent::Finished {
            index,
            report: report.clone(),
        });
        reports.push(report);
    }
    BatchReport { decks: reports }
}

// ---------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------

/// The display copy of a remote-reported value (PRD #1487 audit A3).
fn shown(raw: &str) -> String {
    display_line(raw, REMOTE_NAME_MAX_BYTES)
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "2 agents and 1 orchestration role", the words the desktop's dialog uses
/// for a stop set.
pub fn stop_set_count(set: &RestartStopSet) -> String {
    let mut parts = Vec::new();
    if !set.agents.is_empty() {
        parts.push(count(set.agents.len(), "agent", "agents"));
    }
    if !set.roles.is_empty() {
        parts.push(count(
            set.roles.len(),
            "orchestration role",
            "orchestration roles",
        ));
    }
    if parts.is_empty() {
        "nothing".to_string()
    } else {
        parts.join(" and ")
    }
}

/// One line per agent and role in `set`, the words the desktop's upgrade
/// dialogs use for them (`stopSetLines` in `desktop/src/lib/upgrade.ts`), so
/// the TUI's Upgrade all daemons dialog names them the same way.
pub fn stop_set_lines(set: &RestartStopSet) -> Vec<String> {
    let name = |raw: &str| display_line(raw, REMOTE_NAME_MAX_BYTES);
    let agents = set.agents.iter().map(|agent| {
        let place: Vec<String> = [
            agent
                .pane_id
                .as_deref()
                .map(|p| format!("pane {}", name(p))),
            agent.cwd.as_deref().map(|c| {
                format!(
                    "in {}",
                    display_line(c, crate::untrusted_text::REMOTE_PATH_MAX_BYTES)
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        if place.is_empty() {
            format!("Agent {}", name(&agent.label))
        } else {
            format!("Agent {} ({})", name(&agent.label), place.join(", "))
        }
    });
    let roles = set.roles.iter().map(|role| {
        format!(
            "Role {} of {}, pane {}{}",
            name(&role.role),
            name(&role.orchestration),
            name(&role.pane_id),
            if role.is_orchestrator {
                " (the orchestrator)"
            } else {
                ""
            }
        )
    });
    agents.chain(roles).collect()
}

fn skip_line(reason: &SkipReason) -> String {
    match reason {
        SkipReason::Current { version } => format!("Skipped: already runs {}.", shown(version)),
        SkipReason::DaemonNewer { daemon } => format!(
            "Skipped: runs {}, which is newer than this version.",
            shown(daemon)
        ),
        SkipReason::NoDaemonRunning => "Skipped: no daemon is running there.".to_string(),
        SkipReason::VersionUnknown => {
            "Skipped: the version its daemon runs could not be read.".to_string()
        }
    }
}

fn unreachable_line(reason: &str) -> String {
    format!(
        "Failed: it could not be checked: {}",
        display_message(reason, REMOTE_MESSAGE_MAX_BYTES)
    )
}

fn outcome_line(outcome: &UpgradeOutcome) -> String {
    match outcome {
        UpgradeOutcome::Restarted {
            from_version,
            to_version,
            stopped,
        } => {
            let mut line = format!(
                "Upgraded: now runs {} (was {}).",
                shown(to_version),
                shown(from_version)
            );
            if !stopped.is_empty() {
                line.push_str(&format!(" Stopped {}.", stop_set_count(stopped)));
            }
            line
        }
        UpgradeOutcome::InstalledNotRestarted {
            from_version,
            installed_version,
            reason,
        } => {
            let installed = shown(installed_version);
            let keeps = match from_version {
                Some(v) => format!("the daemon keeps running {}", shown(v)),
                None => "the daemon keeps running".to_string(),
            };
            match reason {
                NotRestartedReason::KeptByUser { .. } => {
                    format!("Kept running: installed {installed}; {keeps}, as you chose.")
                }
                NotRestartedReason::NoOneToAsk { at_stake } => format!(
                    "Kept running: installed {installed}; {keeps}, because restarting it would stop {} and no one was asked.",
                    stop_set_count(at_stake)
                ),
                NotRestartedReason::StaleConfirmation { .. } => format!(
                    "Kept running: installed {installed}; {keeps}, because what was running kept changing while you were asked."
                ),
                NotRestartedReason::AnotherRestartInProgress => format!(
                    "Kept running: installed {installed}; another restart of this daemon was already in progress."
                ),
                NotRestartedReason::NoDaemonRunning => format!(
                    "Installed {installed}: no daemon was running, so the next one to start runs it."
                ),
                NotRestartedReason::InstalledBuildTooOld => format!(
                    "Kept running: installed {installed}; {keeps}, because {installed} is too old to restart it from here."
                ),
                NotRestartedReason::OlderDaemonBusy { at_stake } => format!(
                    "Kept running: {keeps}; it is too old to restart itself, and {} is running on it.",
                    stop_set_count(at_stake)
                ),
            }
        }
        UpgradeOutcome::InstalledDaemonTooOld {
            installed_version,
            daemon_version,
            ..
        } => {
            let daemon = daemon_version
                .as_deref()
                .map(|v| format!(" ({})", shown(v)))
                .unwrap_or_default();
            format!(
                "Kept running: installed {}; the running daemon{daemon} is too old to restart itself.",
                shown(installed_version)
            )
        }
        UpgradeOutcome::Failed { stage, reason, .. } => {
            let doing = match stage {
                UpgradeStage::Installing => "installing the new version",
                UpgradeStage::Restarting => "restarting the daemon",
                UpgradeStage::Verifying => "checking the restarted daemon",
            };
            format!(
                "Failed while {doing}: {}",
                display_message(reason, REMOTE_MESSAGE_MAX_BYTES)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::RestartAgent;
    use crate::daemon_upgrade::{NoDecider, RestartChoice};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CLIENT: &str = "0.52.0";

    fn hello(version: Option<&str>) -> AttachResponse {
        AttachResponse {
            ok: true,
            daemon_version: version.map(str::to_string),
            ..Default::default()
        }
    }

    fn no_binary() -> Result<String, String> {
        panic!("the binary's version must not be read when the daemon named its own")
    }

    fn stop_set(agents: usize) -> RestartStopSet {
        RestartStopSet {
            agents: (0..agents)
                .map(|i| RestartAgent {
                    id: format!("a{i}"),
                    label: format!("agent-{i}"),
                    pane_id: Some(format!("pane-{i}")),
                    cwd: None,
                })
                .collect(),
            roles: Vec::new(),
        }
    }

    fn restarted() -> UpgradeOutcome {
        UpgradeOutcome::Restarted {
            from_version: "0.51.0".into(),
            to_version: CLIENT.into(),
            stopped: RestartStopSet::default(),
        }
    }

    fn failed() -> UpgradeOutcome {
        UpgradeOutcome::Failed {
            stage: UpgradeStage::Installing,
            reason: "the download failed".into(),
            installed_version: None,
            old_daemon_gone: false,
            installed_binary: None,
        }
    }

    /// A deck whose check and upgrade are scripted, recording what it was
    /// asked to do.
    struct Fake {
        name: &'static str,
        check: DeckCheck,
        outcome: UpgradeOutcome,
        /// When set, the upgrade asks the decider about this many agents.
        asks: Option<usize>,
        upgraded: AtomicUsize,
        answers: Mutex<Vec<RestartChoice>>,
    }

    impl Fake {
        fn new(name: &'static str, check: DeckCheck, outcome: UpgradeOutcome) -> Self {
            Self {
                name,
                check,
                outcome,
                asks: None,
                upgraded: AtomicUsize::new(0),
                answers: Mutex::new(Vec::new()),
            }
        }

        fn older(name: &'static str, outcome: UpgradeOutcome) -> Self {
            Self::new(
                name,
                DeckCheck::Upgrade {
                    from: "0.51.0".into(),
                    to: CLIENT.into(),
                },
                outcome,
            )
        }
    }

    impl BatchDeck for Fake {
        fn name(&self) -> &str {
            self.name
        }
        fn check(&self) -> DeckCheck {
            self.check.clone()
        }
        fn upgrade(
            &self,
            decider: &dyn RestartDecider,
            progress: &mut dyn FnMut(UpgradeProgress),
        ) -> UpgradeOutcome {
            self.upgraded.fetch_add(1, Ordering::SeqCst);
            progress(UpgradeProgress {
                stage: UpgradeStage::Installing,
                detail: None,
            });
            if let Some(agents) = self.asks {
                let answer = decider.decide(self.name, &stop_set(agents), false);
                self.answers.lock().unwrap().push(answer);
            }
            self.outcome.clone()
        }
    }

    /// Answers every question with `choice`, recording which deck asked.
    struct Recording {
        choice: RestartChoice,
        asked: Mutex<Vec<(String, usize)>>,
    }

    impl RestartDecider for Recording {
        fn decide(&self, deck: &str, at_stake: &RestartStopSet, _stale: bool) -> RestartChoice {
            self.asked
                .lock()
                .unwrap()
                .push((deck.to_string(), at_stake.agents.len()));
            self.choice
        }
    }

    fn run(decks: &[&dyn BatchDeck], stop: &dyn Fn() -> bool) -> (BatchReport, Vec<BatchEvent>) {
        let plan = check_all(decks);
        let mut events = Vec::new();
        let report = run_plan(decks, &plan, &NoDecider, stop, &mut |e| events.push(e));
        (report, events)
    }

    #[test]
    fn an_older_daemon_is_upgraded_and_the_others_are_skipped_with_their_reason() {
        let check =
            |version: &str| check_from_probe(CLIENT, Ok(Some(hello(Some(version)))), &no_binary);
        assert_eq!(
            check("0.51.0"),
            DeckCheck::Upgrade {
                from: "0.51.0".into(),
                to: CLIENT.into()
            }
        );
        assert_eq!(
            check(CLIENT),
            DeckCheck::Skip {
                reason: SkipReason::Current {
                    version: CLIENT.into()
                }
            }
        );
        assert_eq!(
            check("0.60.0"),
            DeckCheck::Skip {
                reason: SkipReason::DaemonNewer {
                    daemon: "0.60.0".into()
                }
            }
        );
        assert_eq!(
            check("0.1.0+unknown"),
            DeckCheck::Skip {
                reason: SkipReason::VersionUnknown
            }
        );
    }

    #[test]
    fn no_daemon_running_is_skipped_and_an_unreachable_deck_is_not() {
        assert_eq!(
            check_from_probe(CLIENT, Ok(None), &no_binary),
            DeckCheck::Skip {
                reason: SkipReason::NoDaemonRunning
            }
        );
        assert_eq!(
            check_from_probe(
                CLIENT,
                Err(PortError::Other("ssh: connect to host box: refused".into())),
                &no_binary
            ),
            DeckCheck::Unreachable {
                reason: "ssh: connect to host box: refused".into()
            }
        );
    }

    #[test]
    fn a_binary_too_old_to_probe_is_judged_by_its_own_version() {
        let old = check_from_probe(
            CLIENT,
            Err(PortError::InstalledBuildTooOld("no daemon probe".into())),
            &|| Ok("0.40.0".to_string()),
        );
        assert_eq!(
            old,
            DeckCheck::Upgrade {
                from: "0.40.0".into(),
                to: CLIENT.into()
            }
        );
        let gone = check_from_probe(
            CLIENT,
            Err(PortError::InstalledBuildTooOld("no daemon probe".into())),
            &|| Err("dot-agent-deck is not installed there".to_string()),
        );
        assert!(matches!(gone, DeckCheck::Unreachable { .. }));
        // A Hello so old it names no version is older than any versioned
        // client, whatever the installed binary says: an earlier upgrade may
        // have installed a current binary and kept this daemon running.
        let unnamed = check_from_probe(CLIENT, Ok(Some(hello(None))), &no_binary);
        assert_eq!(
            unnamed,
            DeckCheck::Upgrade {
                from: "an older release".into(),
                to: CLIENT.into()
            }
        );
    }

    #[test]
    fn a_failure_does_not_stop_the_batch_and_every_deck_is_reported_in_order() {
        let first = Fake::older("alpha", failed());
        let current = Fake::new(
            "beta",
            DeckCheck::Skip {
                reason: SkipReason::Current {
                    version: CLIENT.into(),
                },
            },
            restarted(),
        );
        let unreachable = Fake::new(
            "gamma",
            DeckCheck::Unreachable {
                reason: "ssh: no route to host".into(),
            },
            restarted(),
        );
        let last = Fake::older("delta", restarted());
        let decks: [&dyn BatchDeck; 4] = [&first, &current, &unreachable, &last];
        let (report, _) = run(&decks, &|| false);

        let statuses: Vec<_> = report
            .decks
            .iter()
            .map(|d| (d.deck.as_str(), d.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("alpha", DeckStatus::Failed),
                ("beta", DeckStatus::Skipped),
                ("gamma", DeckStatus::Failed),
                ("delta", DeckStatus::Upgraded),
            ]
        );
        assert_eq!(first.upgraded.load(Ordering::SeqCst), 1);
        assert_eq!(
            last.upgraded.load(Ordering::SeqCst),
            1,
            "the failure before it must not stop it"
        );
        assert_eq!(current.upgraded.load(Ordering::SeqCst), 0);
        assert_eq!(unreachable.upgraded.load(Ordering::SeqCst), 0);
        assert!(report.is_failure());
        assert_eq!(
            report.headline(),
            "Upgrade all: 1 upgraded, 1 skipped, 2 failed of 4 remote decks."
        );
        let summary = report.summary();
        assert!(
            summary.contains("alpha: Failed while installing the new version: the download failed"),
            "{summary}"
        );
        assert!(
            summary.contains("beta: Skipped: already runs 0.52.0."),
            "{summary}"
        );
        assert!(
            summary.contains("gamma: Failed: it could not be checked: ssh: no route to host"),
            "{summary}"
        );
        assert!(
            summary.contains("delta: Upgraded: now runs 0.52.0 (was 0.51.0)."),
            "{summary}"
        );
    }

    #[test]
    fn skipped_and_kept_decks_are_not_failures() {
        let kept = Fake::older(
            "alpha",
            UpgradeOutcome::InstalledNotRestarted {
                from_version: Some("0.51.0".into()),
                installed_version: CLIENT.into(),
                reason: NotRestartedReason::NoOneToAsk {
                    at_stake: stop_set(2),
                },
            },
        );
        let newer = Fake::new(
            "beta",
            DeckCheck::Skip {
                reason: SkipReason::DaemonNewer {
                    daemon: "0.60.0".into(),
                },
            },
            restarted(),
        );
        let decks: [&dyn BatchDeck; 2] = [&kept, &newer];
        let (report, _) = run(&decks, &|| false);
        assert!(!report.is_failure());
        assert_eq!(report.decks[0].status, DeckStatus::Kept);
        assert_eq!(
            report.decks[0].line,
            "Kept running: installed 0.52.0; the daemon keeps running 0.51.0, because restarting it would stop 2 agents and no one was asked."
        );
        assert_eq!(
            report.decks[1].line,
            "Skipped: runs 0.60.0, which is newer than this version."
        );
    }

    #[test]
    fn each_daemon_with_live_work_is_asked_about_its_own() {
        let mut alpha = Fake::older("alpha", restarted());
        alpha.asks = Some(1);
        let idle = Fake::older("beta", restarted());
        let mut gamma = Fake::older("gamma", restarted());
        gamma.asks = Some(3);
        let decks: [&dyn BatchDeck; 3] = [&alpha, &idle, &gamma];
        let plan = check_all(&decks);
        let decider = Recording {
            choice: RestartChoice::KeepCurrent,
            asked: Mutex::new(Vec::new()),
        };
        run_plan(&decks, &plan, &decider, &|| false, &mut |_| {});
        assert_eq!(
            *decider.asked.lock().unwrap(),
            [("alpha".to_string(), 1), ("gamma".to_string(), 3)],
            "one question per busy daemon, naming its own work; none for the idle one"
        );
    }

    #[test]
    fn stopping_finishes_the_current_deck_and_starts_no_other() {
        let first = Fake::older("alpha", restarted());
        let second = Fake::older("beta", restarted());
        let skipped = Fake::new(
            "gamma",
            DeckCheck::Skip {
                reason: SkipReason::NoDaemonRunning,
            },
            restarted(),
        );
        let decks: [&dyn BatchDeck; 3] = [&first, &second, &skipped];
        // Stop is asked before each deck starts: no to the first, yes after.
        let asked = AtomicUsize::new(0);
        let (report, events) = run(&decks, &|| asked.fetch_add(1, Ordering::SeqCst) > 0);
        assert_eq!(first.upgraded.load(Ordering::SeqCst), 1);
        assert_eq!(second.upgraded.load(Ordering::SeqCst), 0);
        assert_eq!(report.decks[1].result, DeckResult::NotStarted);
        assert_eq!(report.decks[1].status, DeckStatus::Skipped);
        assert_eq!(
            report.decks[2].result,
            DeckResult::Skipped {
                reason: SkipReason::NoDaemonRunning
            },
            "a deck the check skipped keeps its own reason"
        );
        assert!(!report.is_failure());
        let started: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                BatchEvent::Started { deck, .. } => Some(deck.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(started, ["alpha"]);
        let finished = events
            .iter()
            .filter(|e| matches!(e, BatchEvent::Finished { .. }))
            .count();
        assert_eq!(finished, 3, "every deck's part ends with a Finished event");
    }

    #[test]
    fn the_events_of_an_upgrade_come_in_order() {
        let only = Fake::older("alpha", restarted());
        let decks: [&dyn BatchDeck; 1] = [&only];
        let (_, events) = run(&decks, &|| false);
        assert!(matches!(&events[0], BatchEvent::Started { index: 0, deck } if deck == "alpha"));
        assert!(matches!(&events[1], BatchEvent::Progress { index: 0, .. }));
        assert!(
            matches!(&events[2], BatchEvent::Finished { index: 0, report } if report.status == DeckStatus::Upgraded)
        );
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn decks_are_checked_at_the_same_time() {
        // The two checks wait for each other at one barrier: run one after the
        // other, the first would wait forever, so finishing proves they overlap.
        struct Meeting<'a>(&'static str, &'a std::sync::Barrier);
        impl BatchDeck for Meeting<'_> {
            fn name(&self) -> &str {
                self.0
            }
            fn check(&self) -> DeckCheck {
                self.1.wait();
                DeckCheck::Skip {
                    reason: SkipReason::NoDaemonRunning,
                }
            }
            fn upgrade(
                &self,
                _: &dyn RestartDecider,
                _: &mut dyn FnMut(UpgradeProgress),
            ) -> UpgradeOutcome {
                unreachable!("a skipped deck is never upgraded")
            }
        }
        let both = std::sync::Barrier::new(2);
        let (x, y) = (Meeting("x", &both), Meeting("y", &both));
        let decks: [&dyn BatchDeck; 2] = [&x, &y];
        let plan = check_all(&decks);
        let names: Vec<_> = plan.decks.iter().map(|p| p.deck.as_str()).collect();
        assert_eq!(names, ["x", "y"], "the plan keeps deck-list order");
    }

    #[test]
    fn a_long_deck_list_is_checked_a_bounded_number_at_a_time_in_order() {
        struct Counting<'a> {
            name: String,
            running: &'a AtomicUsize,
            most: &'a AtomicUsize,
        }
        impl BatchDeck for Counting<'_> {
            fn name(&self) -> &str {
                &self.name
            }
            fn check(&self) -> DeckCheck {
                let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                self.running.fetch_sub(1, Ordering::SeqCst);
                DeckCheck::Skip {
                    reason: SkipReason::NoDaemonRunning,
                }
            }
            fn upgrade(
                &self,
                _: &dyn RestartDecider,
                _: &mut dyn FnMut(UpgradeProgress),
            ) -> UpgradeOutcome {
                unreachable!("a skipped deck is never upgraded")
            }
        }
        let (running, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let decks: Vec<Counting> = (0..MAX_PARALLEL_CHECKS * 2 + 3)
            .map(|i| Counting {
                name: format!("deck-{i}"),
                running: &running,
                most: &most,
            })
            .collect();
        let refs: Vec<&dyn BatchDeck> = decks.iter().map(|d| d as &dyn BatchDeck).collect();
        let plan = check_all(&refs);
        assert!(most.load(Ordering::SeqCst) <= MAX_PARALLEL_CHECKS);
        let names: Vec<_> = plan.decks.iter().map(|p| p.deck.clone()).collect();
        let wanted: Vec<_> = decks.iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, wanted);
    }

    #[test]
    fn an_empty_deck_list_says_so_and_is_not_a_failure() {
        let (report, events) = run(&[], &|| false);
        assert!(events.is_empty());
        assert!(!report.is_failure());
        assert_eq!(report.summary(), NO_REMOTE_DECKS);
        assert_eq!(check_all(&[]).summary(), NO_REMOTE_DECKS);
    }

    #[test]
    fn the_plan_names_each_deck_and_what_will_happen_to_it() {
        let plan = BatchPlan {
            decks: vec![
                PlannedDeck {
                    deck: "alpha".into(),
                    check: DeckCheck::Upgrade {
                        from: "0.51.0".into(),
                        to: CLIENT.into(),
                    },
                },
                PlannedDeck {
                    deck: "beta".into(),
                    check: DeckCheck::Skip {
                        reason: SkipReason::NoDaemonRunning,
                    },
                },
            ],
        };
        assert_eq!(plan.to_upgrade(), 1);
        assert_eq!(
            plan.summary(),
            "Checked 2 remote decks: 1 to upgrade.\n  alpha: Will be upgraded from 0.51.0 to 0.52.0.\n  beta: Skipped: no daemon is running there."
        );
    }

    #[test]
    fn remote_text_cannot_steer_the_terminal() {
        let report = BatchReport {
            decks: vec![DeckReport::new(
                "alpha",
                DeckResult::Unreachable {
                    reason: "bad\x1b[2Jthing\nsecond".into(),
                },
            )],
        };
        let summary = report.summary();
        assert!(!summary.contains('\x1b'), "{summary:?}");
    }

    #[test]
    fn the_report_serialises_with_its_status_and_line() {
        let report = BatchReport {
            decks: vec![DeckReport::new(
                "alpha",
                DeckResult::Ran {
                    outcome: restarted(),
                },
            )],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["decks"][0]["deck"], "alpha");
        assert_eq!(json["decks"][0]["status"], "upgraded");
        assert_eq!(json["decks"][0]["result"]["kind"], "ran");
        assert_eq!(
            json["decks"][0]["result"]["outcome"]["outcome"],
            "restarted"
        );
        let back: BatchReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, report);
    }
}
