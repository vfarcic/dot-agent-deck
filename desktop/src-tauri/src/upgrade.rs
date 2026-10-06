//! PRD #1487 M5: the desktop's **Upgrade** action and the local **Replace
//! daemon**, both on the root crate's one upgrade procedure,
//! [`dot_agent_deck::daemon_upgrade::upgrade_daemon`].
//!
//! Nothing here decides anything about the upgrade (PRD D1). What this module
//! owns is the desktop's half of the conversation:
//!
//! - which installer and which daemon port a deck gets — a remote deck installs
//!   over SSH and restarts through the freshly installed binary
//!   ([`RestartSuccessor::Installed`]); the local deck installs nothing and the
//!   app starts its own bundled build once the old daemon has gone
//!   ([`RestartSuccessor::ClientSpawns`], D10);
//! - the restart question, asked through a dialog: [`DesktopDecider`] emits
//!   `desktop://upgrade-decision` and blocks until `desktop_upgrade_decide`
//!   answers. Every way of not answering — the dialog closed, the webview gone,
//!   the app quitting, nobody answering for [`DECISION_TIMEOUT`] — is **Keep
//!   current daemon**, so a restart only ever happens on an explicit
//!   "Restart now";
//! - one upgrade per deck at a time (the daemon serialises across clients
//!   anyway; this keeps a second press from starting a second install);
//! - the outcome as a camelCase DTO for the webview.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dot_agent_deck::daemon_client::Endpoint;
use dot_agent_deck::daemon_protocol::{RestartAgent, RestartStopSet, RestartSuccessor};
use dot_agent_deck::daemon_upgrade::{
    CLIENT_VERSION, NotRestartedReason, RestartChoice, RestartDecider, UpgradeOutcome,
    UpgradeProgress, UpgradeStage,
};
use dot_agent_deck::remote::{RemoteEntry, RemotesFile};
use dot_agent_deck::state::OrchestrationRoleRecord;
use serde::Serialize;

use crate::dto::safe_message;

/// The event a running upgrade reports its stage on.
pub(crate) const PROGRESS_EVENT: &str = "desktop://upgrade-progress";
/// The event that asks the webview the restart question.
pub(crate) const DECISION_EVENT: &str = "desktop://upgrade-decision";

/// How long the restart question waits for an answer before it is taken as
/// **Keep current daemon**. Long enough for a person to read the list and
/// decide; bounded so a webview that reloaded mid-question (and so lost the
/// dialog) cannot hold the upgrade, and the deck's in-flight slot, forever.
pub(crate) const DECISION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What the decision dialog sends back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecisionChoice {
    RestartNow,
    KeepCurrent,
}

impl DecisionChoice {
    /// The wire spelling `desktop_upgrade_decide` takes.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "restart-now" => Ok(Self::RestartNow),
            "keep-current" => Ok(Self::KeepCurrent),
            other => Err(format!(
                "unknown choice {:?}: expected \"restart-now\" or \"keep-current\"",
                safe_message(other)
            )),
        }
    }
}

/// The upgrades this app is running, and the questions they are waiting on.
#[derive(Default, Clone)]
pub(crate) struct UpgradeState {
    inner: Arc<UpgradeInner>,
}

#[derive(Default)]
struct UpgradeInner {
    /// Questions waiting for `desktop_upgrade_decide`, by upgrade id. Dropping
    /// a sender is an answer: the waiting decider reads it as Keep.
    pending: Mutex<HashMap<String, mpsc::Sender<DecisionChoice>>>,
    /// Decks with an upgrade running, by wire deck id.
    in_flight: Mutex<HashSet<String>>,
    next_id: AtomicU64,
}

/// A deck's in-flight slot; released on drop, including on a panic or an early
/// return, so a failed upgrade never leaves the button refusing forever.
pub(crate) struct InFlight {
    inner: Arc<UpgradeInner>,
    deck_id: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        lock(&self.inner.in_flight).remove(&self.deck_id);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the webview reads when a second press lands on a deck already being
/// upgraded.
pub(crate) const ALREADY_RUNNING: &str =
    "An upgrade of this daemon is already running in this app. Wait for it to finish.";

impl UpgradeState {
    /// Claim `deck_id`'s slot, or refuse while another upgrade holds it.
    pub(crate) fn begin(&self, deck_id: &str) -> Result<InFlight, String> {
        if !lock(&self.inner.in_flight).insert(deck_id.to_string()) {
            return Err(ALREADY_RUNNING.into());
        }
        Ok(InFlight {
            inner: Arc::clone(&self.inner),
            deck_id: deck_id.to_string(),
        })
    }

    /// A fresh id for one upgrade run, unique for the life of the process.
    pub(crate) fn next_upgrade_id(&self) -> String {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("upgrade-{n}")
    }

    /// Answer the question `upgrade_id` is waiting on.
    pub(crate) fn decide(&self, upgrade_id: &str, choice: DecisionChoice) -> Result<(), String> {
        let sender = lock(&self.inner.pending)
            .remove(upgrade_id)
            .ok_or_else(|| {
                "That upgrade is no longer waiting for an answer; it may have finished already."
                    .to_string()
            })?;
        // A send can only fail if the decider stopped waiting (its timeout),
        // which already took the safe answer.
        let _ = sender.send(choice);
        Ok(())
    }

    /// Drop every waiting question, so each reads as Keep current daemon. Run
    /// on app exit.
    pub(crate) fn abandon_all(&self) {
        lock(&self.inner.pending).clear();
    }

    fn register(&self, upgrade_id: &str) -> mpsc::Receiver<DecisionChoice> {
        let (tx, rx) = mpsc::channel();
        lock(&self.inner.pending).insert(upgrade_id.to_string(), tx);
        rx
    }

    fn forget(&self, upgrade_id: &str) {
        lock(&self.inner.pending).remove(upgrade_id);
    }

    #[cfg(test)]
    fn waiting(&self) -> usize {
        lock(&self.inner.pending).len()
    }
}

// ---------------------------------------------------------------------------
// Events and DTOs
// ---------------------------------------------------------------------------

/// `desktop://upgrade-progress`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpgradeProgressEvent {
    pub deck_id: String,
    pub upgrade_id: String,
    pub progress: UpgradeProgress,
}

/// `desktop://upgrade-decision`: "restarting stops these; restart now?".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpgradeDecisionEvent {
    pub deck_id: String,
    pub upgrade_id: String,
    pub at_stake: StopSetDto,
    /// What would stop changed since the last time this run asked.
    pub stale: bool,
}

/// What a restart stops, as the webview names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StopSetDto {
    pub agents: Vec<StopAgentDto>,
    pub roles: Vec<StopRoleDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StopAgentDto {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StopRoleDto {
    pub pane_id: String,
    pub role: String,
    pub orchestration: String,
    pub is_orchestrator: bool,
}

impl From<&RestartAgent> for StopAgentDto {
    fn from(agent: &RestartAgent) -> Self {
        Self {
            id: safe_message(&agent.id),
            label: safe_message(&agent.label),
            pane_id: agent.pane_id.as_deref().map(safe_message),
            cwd: agent.cwd.as_deref().map(safe_message),
        }
    }
}

impl From<&OrchestrationRoleRecord> for StopRoleDto {
    fn from(role: &OrchestrationRoleRecord) -> Self {
        Self {
            pane_id: safe_message(&role.pane_id),
            role: safe_message(&role.role),
            orchestration: safe_message(&role.orchestration),
            is_orchestrator: role.is_orchestrator,
        }
    }
}

impl From<&RestartStopSet> for StopSetDto {
    fn from(set: &RestartStopSet) -> Self {
        Self {
            agents: set.agents.iter().map(StopAgentDto::from).collect(),
            roles: set.roles.iter().map(StopRoleDto::from).collect(),
        }
    }
}

/// [`NotRestartedReason`], camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum NotRestartedDto {
    #[serde(rename_all = "camelCase")]
    KeptByUser {
        at_stake: StopSetDto,
    },
    #[serde(rename_all = "camelCase")]
    NoOneToAsk {
        at_stake: StopSetDto,
    },
    #[serde(rename_all = "camelCase")]
    StaleConfirmation {
        at_stake: StopSetDto,
    },
    AnotherRestartInProgress,
    NoDaemonRunning,
    InstalledBuildTooOld,
    #[serde(rename_all = "camelCase")]
    OlderDaemonBusy {
        at_stake: StopSetDto,
    },
}

/// [`UpgradeOutcome`], camelCase — what `desktop_upgrade_daemon` resolves with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub(crate) enum UpgradeOutcomeDto {
    #[serde(rename_all = "camelCase")]
    Restarted {
        from_version: String,
        to_version: String,
        stopped: StopSetDto,
    },
    #[serde(rename_all = "camelCase")]
    InstalledNotRestarted {
        #[serde(skip_serializing_if = "Option::is_none")]
        from_version: Option<String>,
        installed_version: String,
        reason: NotRestartedDto,
    },
    #[serde(rename_all = "camelCase")]
    InstalledDaemonTooOld {
        installed_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        daemon_version: Option<String>,
        remedy: String,
    },
    #[serde(rename_all = "camelCase")]
    Failed {
        stage: UpgradeStage,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        installed_version: Option<String>,
    },
}

impl From<&UpgradeOutcome> for UpgradeOutcomeDto {
    fn from(outcome: &UpgradeOutcome) -> Self {
        let text = |value: &String| safe_message(value);
        match outcome {
            UpgradeOutcome::Restarted {
                from_version,
                to_version,
                stopped,
            } => Self::Restarted {
                from_version: text(from_version),
                to_version: text(to_version),
                stopped: stopped.into(),
            },
            UpgradeOutcome::InstalledNotRestarted {
                from_version,
                installed_version,
                reason,
            } => Self::InstalledNotRestarted {
                from_version: from_version.as_ref().map(text),
                installed_version: text(installed_version),
                reason: match reason {
                    NotRestartedReason::KeptByUser { at_stake } => NotRestartedDto::KeptByUser {
                        at_stake: at_stake.into(),
                    },
                    NotRestartedReason::NoOneToAsk { at_stake } => NotRestartedDto::NoOneToAsk {
                        at_stake: at_stake.into(),
                    },
                    NotRestartedReason::StaleConfirmation { at_stake } => {
                        NotRestartedDto::StaleConfirmation {
                            at_stake: at_stake.into(),
                        }
                    }
                    NotRestartedReason::AnotherRestartInProgress => {
                        NotRestartedDto::AnotherRestartInProgress
                    }
                    NotRestartedReason::NoDaemonRunning => NotRestartedDto::NoDaemonRunning,
                    NotRestartedReason::InstalledBuildTooOld => {
                        NotRestartedDto::InstalledBuildTooOld
                    }
                    NotRestartedReason::OlderDaemonBusy { at_stake } => {
                        NotRestartedDto::OlderDaemonBusy {
                            at_stake: at_stake.into(),
                        }
                    }
                },
            },
            UpgradeOutcome::InstalledDaemonTooOld {
                installed_version,
                daemon_version,
                remedy,
            } => Self::InstalledDaemonTooOld {
                installed_version: text(installed_version),
                daemon_version: daemon_version.as_ref().map(text),
                remedy: text(remedy),
            },
            UpgradeOutcome::Failed {
                stage,
                reason,
                installed_version,
            } => Self::Failed {
                stage: *stage,
                reason: text(reason),
                installed_version: installed_version.as_ref().map(text),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// The decider
// ---------------------------------------------------------------------------

/// Asks the restart question in the webview. Called on the blocking thread the
/// upgrade runs on, so it may block on the answer.
pub(crate) struct DesktopDecider {
    state: UpgradeState,
    deck_id: String,
    upgrade_id: String,
    emit: Box<dyn Fn(&UpgradeDecisionEvent) + Send + Sync>,
    timeout: Duration,
}

impl DesktopDecider {
    pub(crate) fn new(
        state: UpgradeState,
        deck_id: String,
        upgrade_id: String,
        emit: Box<dyn Fn(&UpgradeDecisionEvent) + Send + Sync>,
    ) -> Self {
        Self {
            state,
            deck_id,
            upgrade_id,
            emit,
            timeout: DECISION_TIMEOUT,
        }
    }
}

impl RestartDecider for DesktopDecider {
    fn decide(&self, _deck: &str, at_stake: &RestartStopSet, stale: bool) -> RestartChoice {
        // Registered BEFORE the event goes out, so an answer that arrives at
        // once still finds the question.
        let answer = self.state.register(&self.upgrade_id);
        (self.emit)(&UpgradeDecisionEvent {
            deck_id: self.deck_id.clone(),
            upgrade_id: self.upgrade_id.clone(),
            at_stake: at_stake.into(),
            stale,
        });
        let choice = answer.recv_timeout(self.timeout);
        self.state.forget(&self.upgrade_id);
        match choice {
            Ok(DecisionChoice::RestartNow) => RestartChoice::RestartNow,
            // Keep, a closed dialog (the sender dropped), app exit, or no
            // answer in time: nothing is stopped without an explicit yes.
            Ok(DecisionChoice::KeepCurrent) | Err(_) => RestartChoice::KeepCurrent,
        }
    }
}

// ---------------------------------------------------------------------------
// Which deck, and how to reach it
// ---------------------------------------------------------------------------

/// What the upgrade of one deck runs with.
pub(crate) enum UpgradeTarget {
    /// A deck from the shared deck list: install over SSH, restart onto the
    /// installed build.
    Remote(Box<RemoteEntry>),
    /// This machine's daemon: the app's own build replaces it (Replace daemon).
    Local,
}

impl UpgradeTarget {
    pub(crate) fn successor(&self) -> RestartSuccessor {
        match self {
            Self::Remote(_) => RestartSuccessor::Installed,
            Self::Local => RestartSuccessor::ClientSpawns,
        }
    }

    /// The name the procedure uses for the deck in what it reports.
    pub(crate) fn deck_name(&self) -> String {
        match self {
            Self::Remote(entry) => entry.name.clone(),
            Self::Local => "this machine".into(),
        }
    }
}

/// Said when a remote deck the app shows is not in the deck list, so there is
/// no SSH route to install over.
pub(crate) const NOT_IN_DECK_LIST: &str = "This daemon is not in the deck list (remotes.toml), so the app does not know how to reach its machine to install the new version. Add it with `dot-agent-deck remote add`, or upgrade it from a terminal with `dot-agent-deck remote upgrade`.";

/// The deck-list row `endpoint` was built from — the same row `remote upgrade`
/// would look up by name, matched here by the endpoint it describes.
pub(crate) fn remote_entry_for(endpoint: &Endpoint, path: &Path) -> Result<RemoteEntry, String> {
    let identity = endpoint.identity();
    let file = RemotesFile::load(path).map_err(|error| safe_message(error.to_string()))?;
    file.remotes
        .into_iter()
        .find(|entry| {
            crate::decks::row_from_entry(entry)
                .ok()
                .and_then(|row| row.endpoint())
                .is_some_and(|remote| Endpoint::Remote(remote).identity() == identity)
        })
        .ok_or_else(|| NOT_IN_DECK_LIST.to_string())
}

/// The version an upgrade installs: the app's own (PRD D11).
pub(crate) fn plan_version() -> String {
    CLIENT_VERSION.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn stop_set() -> RestartStopSet {
        RestartStopSet {
            agents: vec![RestartAgent {
                id: "7".into(),
                label: "coder".into(),
                pane_id: Some("3".into()),
                cwd: Some("/work/app".into()),
            }],
            roles: vec![OrchestrationRoleRecord {
                pane_id: "3".into(),
                role: "coder".into(),
                orchestration: "tdd".into(),
                is_orchestrator: false,
            }],
        }
    }

    /// A decider whose emitted questions are answered by `answer` on another
    /// thread, the way the webview answers through `desktop_upgrade_decide`.
    fn decider_answering(
        state: &UpgradeState,
        answer: Option<DecisionChoice>,
        timeout: Duration,
    ) -> (DesktopDecider, Arc<Mutex<Vec<UpgradeDecisionEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = Arc::clone(&seen);
        let answering = state.clone();
        let mut decider = DesktopDecider::new(
            state.clone(),
            "deck-1".into(),
            "upgrade-1".into(),
            Box::new(move |event: &UpgradeDecisionEvent| {
                lock(&seen_in).push(event.clone());
                if let Some(choice) = answer {
                    let answering = answering.clone();
                    let id = event.upgrade_id.clone();
                    thread::spawn(move || answering.decide(&id, choice).unwrap());
                }
            }),
        );
        decider.timeout = timeout;
        (decider, seen)
    }

    #[test]
    fn restart_now_from_the_dialog_is_restart_now() {
        let state = UpgradeState::default();
        let (decider, seen) = decider_answering(
            &state,
            Some(DecisionChoice::RestartNow),
            Duration::from_secs(5),
        );
        assert_eq!(
            decider.decide("box", &stop_set(), false),
            RestartChoice::RestartNow
        );
        let events = lock(&seen);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].deck_id, "deck-1");
        assert_eq!(events[0].at_stake, StopSetDto::from(&stop_set()));
        assert!(!events[0].stale);
        assert_eq!(state.waiting(), 0, "the answered question is forgotten");
    }

    #[test]
    fn keep_from_the_dialog_is_keep() {
        let state = UpgradeState::default();
        let (decider, _) = decider_answering(
            &state,
            Some(DecisionChoice::KeepCurrent),
            Duration::from_secs(5),
        );
        assert_eq!(
            decider.decide("box", &stop_set(), true),
            RestartChoice::KeepCurrent
        );
    }

    /// Nobody answers: the question times out as Keep, and is forgotten so a
    /// late answer is refused rather than taken.
    #[test]
    fn an_unanswered_question_is_keep() {
        let state = UpgradeState::default();
        let (decider, _) = decider_answering(&state, None, Duration::from_millis(20));
        assert_eq!(
            decider.decide("box", &stop_set(), false),
            RestartChoice::KeepCurrent
        );
        assert_eq!(state.waiting(), 0);
        assert!(
            state
                .decide("upgrade-1", DecisionChoice::RestartNow)
                .is_err()
        );
    }

    /// App exit drops every waiting question; the decider reads Keep at once
    /// rather than waiting out its timeout.
    #[test]
    fn app_exit_answers_keep() {
        let state = UpgradeState::default();
        let exiting = state.clone();
        let mut decider = DesktopDecider::new(
            state.clone(),
            "deck-1".into(),
            "upgrade-1".into(),
            Box::new(move |_: &UpgradeDecisionEvent| exiting.abandon_all()),
        );
        decider.timeout = Duration::from_secs(60);
        let started = std::time::Instant::now();
        assert_eq!(
            decider.decide("box", &stop_set(), false),
            RestartChoice::KeepCurrent
        );
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn an_answer_for_no_question_is_refused() {
        let state = UpgradeState::default();
        let error = state
            .decide("upgrade-9", DecisionChoice::KeepCurrent)
            .unwrap_err();
        assert!(error.contains("no longer waiting"), "{error}");
    }

    #[test]
    fn choices_parse_from_their_wire_spelling_only() {
        assert_eq!(
            DecisionChoice::parse("restart-now"),
            Ok(DecisionChoice::RestartNow)
        );
        assert_eq!(
            DecisionChoice::parse("keep-current"),
            Ok(DecisionChoice::KeepCurrent)
        );
        assert!(DecisionChoice::parse("restart_now").is_err());
        assert!(DecisionChoice::parse("").is_err());
    }

    #[test]
    fn a_second_upgrade_of_one_deck_is_refused_until_the_first_ends() {
        let state = UpgradeState::default();
        let first = state.begin("deck-1").unwrap();
        assert_eq!(
            state.begin("deck-1").err().as_deref(),
            Some(ALREADY_RUNNING)
        );
        // Another deck is independent.
        let other = state.begin("deck-2").unwrap();
        drop(first);
        let again = state.begin("deck-1");
        assert!(again.is_ok());
        drop(other);
    }

    #[test]
    fn upgrade_ids_are_unique() {
        let state = UpgradeState::default();
        let a = state.next_upgrade_id();
        let b = state.next_upgrade_id();
        assert_ne!(a, b);
    }

    #[test]
    fn the_outcome_dto_is_camel_case_for_every_arm() {
        let restarted = serde_json::to_value(UpgradeOutcomeDto::from(&UpgradeOutcome::Restarted {
            from_version: "0.44.0".into(),
            to_version: "0.45.0".into(),
            stopped: stop_set(),
        }))
        .unwrap();
        assert_eq!(restarted["outcome"], "restarted");
        assert_eq!(restarted["fromVersion"], "0.44.0");
        assert_eq!(restarted["stopped"]["agents"][0]["paneId"], "3");
        assert_eq!(restarted["stopped"]["roles"][0]["isOrchestrator"], false);

        let kept = serde_json::to_value(UpgradeOutcomeDto::from(
            &UpgradeOutcome::InstalledNotRestarted {
                from_version: Some("0.44.0".into()),
                installed_version: "0.45.0".into(),
                reason: NotRestartedReason::KeptByUser {
                    at_stake: stop_set(),
                },
            },
        ))
        .unwrap();
        assert_eq!(kept["outcome"], "installed-not-restarted");
        assert_eq!(kept["installedVersion"], "0.45.0");
        assert_eq!(kept["reason"]["kind"], "kept-by-user");
        assert_eq!(kept["reason"]["atStake"]["agents"][0]["label"], "coder");

        let idle = serde_json::to_value(UpgradeOutcomeDto::from(
            &UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: "0.45.0".into(),
                reason: NotRestartedReason::NoDaemonRunning,
            },
        ))
        .unwrap();
        assert_eq!(idle["reason"]["kind"], "no-daemon-running");
        assert!(idle.get("fromVersion").is_none());

        let older_build = serde_json::to_value(UpgradeOutcomeDto::from(
            &UpgradeOutcome::InstalledNotRestarted {
                from_version: None,
                installed_version: "0.40.0".into(),
                reason: NotRestartedReason::InstalledBuildTooOld,
            },
        ))
        .unwrap();
        assert_eq!(older_build["reason"]["kind"], "installed-build-too-old");

        let busy = serde_json::to_value(UpgradeOutcomeDto::from(
            &UpgradeOutcome::InstalledNotRestarted {
                from_version: Some("0.44.0".into()),
                installed_version: "0.45.0".into(),
                reason: NotRestartedReason::OlderDaemonBusy {
                    at_stake: stop_set(),
                },
            },
        ))
        .unwrap();
        assert_eq!(busy["reason"]["kind"], "older-daemon-busy");
        assert_eq!(busy["reason"]["atStake"]["agents"][0]["label"], "coder");

        let too_old = serde_json::to_value(UpgradeOutcomeDto::from(
            &UpgradeOutcome::InstalledDaemonTooOld {
                installed_version: "0.45.0".into(),
                daemon_version: Some("0.30.0".into()),
                remedy: "connect\u{7}".into(),
            },
        ))
        .unwrap();
        assert_eq!(too_old["outcome"], "installed-daemon-too-old");
        assert_eq!(too_old["daemonVersion"], "0.30.0");
        assert_eq!(too_old["remedy"], "connect", "control characters scrubbed");

        let failed = serde_json::to_value(UpgradeOutcomeDto::from(&UpgradeOutcome::Failed {
            stage: UpgradeStage::Installing,
            reason: "ssh: connect timed out".into(),
            installed_version: None,
        }))
        .unwrap();
        assert_eq!(failed["outcome"], "failed");
        assert_eq!(failed["stage"], "installing");
        assert!(failed.get("installedVersion").is_none());
    }

    #[test]
    fn the_progress_and_decision_events_are_camel_case() {
        let progress = serde_json::to_value(UpgradeProgressEvent {
            deck_id: "deck-1".into(),
            upgrade_id: "upgrade-1".into(),
            progress: UpgradeProgress {
                stage: UpgradeStage::Verifying,
                detail: None,
            },
        })
        .unwrap();
        assert_eq!(progress["deckId"], "deck-1");
        assert_eq!(progress["upgradeId"], "upgrade-1");
        assert_eq!(progress["progress"]["stage"], "verifying");

        let decision = serde_json::to_value(UpgradeDecisionEvent {
            deck_id: "deck-1".into(),
            upgrade_id: "upgrade-1".into(),
            at_stake: (&stop_set()).into(),
            stale: true,
        })
        .unwrap();
        assert_eq!(decision["atStake"]["roles"][0]["orchestration"], "tdd");
        assert_eq!(decision["stale"], true);
    }

    #[test]
    fn a_local_target_spawns_its_own_build_and_a_remote_one_restarts_onto_the_install() {
        assert_eq!(
            UpgradeTarget::Local.successor(),
            RestartSuccessor::ClientSpawns
        );
        let entry: RemoteEntry = toml_edit::de::from_str(
            r#"
            name = "build-box"
            type = "ssh"
            host = "build-box"
            port = 22
            version = "0.44.0"
            added_at = "2026-10-01T00:00:00Z"
            "#,
        )
        .unwrap();
        let remote = UpgradeTarget::Remote(Box::new(entry));
        assert_eq!(remote.successor(), RestartSuccessor::Installed);
        assert_eq!(remote.deck_name(), "build-box");
    }

    /// The row is found by the endpoint it describes, and a deck that is in no
    /// row is refused with a sentence saying what to do.
    #[test]
    fn the_deck_list_row_is_found_by_its_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        std::fs::write(
            &path,
            r#"
            [[remotes]]
            name = "build-box"
            type = "ssh"
            host = "dev@build-box"
            port = 22
            socket = "/run/user/1000/dot-agent-deck/attach.sock"
            version = "0.44.0"
            added_at = "2026-10-01T00:00:00Z"

            [[remotes]]
            name = "other"
            type = "ssh"
            host = "other-box"
            port = 22
            socket = "/run/user/1000/dot-agent-deck/attach.sock"
            version = "0.44.0"
            added_at = "2026-10-01T00:00:00Z"
            "#,
        )
        .unwrap();
        let file = RemotesFile::load(&path).unwrap();
        let row = crate::decks::row_from_entry(&file.remotes[1]).unwrap();
        let endpoint = Endpoint::Remote(row.endpoint().expect("connectable"));
        assert_eq!(remote_entry_for(&endpoint, &path).unwrap().name, "other");

        let missing = Endpoint::Remote(
            crate::decks::row_from_entry(&file.remotes[0])
                .unwrap()
                .endpoint()
                .unwrap(),
        );
        std::fs::write(&path, "").unwrap();
        assert_eq!(
            remote_entry_for(&missing, &path).unwrap_err(),
            NOT_IN_DECK_LIST
        );
    }
}
