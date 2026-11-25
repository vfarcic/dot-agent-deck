//! Issue #1600: the desktop's **Upgrade all** — every remote deck on the deck
//! list checked, then the older ones upgraded one after another, through the
//! root crate's batch, [`dot_agent_deck::daemon_upgrade_batch`], which the
//! CLI's `remote upgrade --all` and the TUI's Upgrade all daemons dialog run
//! too.
//!
//! Nothing here decides anything about the batch (PRD #1487 D1, CLAUDE.md rule
//! 18): which deck is upgraded, what stops the batch and what each result is
//! called are the crate's. What this module owns is the desktop's half:
//!
//! - the checked plan, kept here by its batch id between
//!   `desktop_upgrade_all_check` and `desktop_upgrade_all_run`, so the run
//!   upgrades exactly the decks the user was shown and the webview never sends
//!   a list of its own;
//! - each daemon's restart question, asked through the same
//!   `desktop://upgrade-decision` event and answered by the same
//!   `desktop_upgrade_decide` a single Upgrade uses ([`BatchDecider`]), naming
//!   the deck that asks;
//! - **Stop after this daemon** (`desktop_upgrade_all_stop`);
//! - the plan, each event and the report as camelCase DTOs, carrying the
//!   crate's own words for every line.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dot_agent_deck::daemon_client::Endpoint;
use dot_agent_deck::daemon_protocol::RestartStopSet;
use dot_agent_deck::daemon_upgrade::{RestartChoice, RestartDecider, UpgradeStage};
use dot_agent_deck::daemon_upgrade_batch::{
    BatchDeck, BatchEvent, BatchPlan, BatchReport, DeckCheck, DeckReport, DeckResult, DeckStatus,
    RemoteBatchDeck, check_all, no_report,
};
use dot_agent_deck::remote::RemoteEntry;
use serde::Serialize;

use crate::dto::safe_message;
use crate::upgrade::{DesktopDecider, InFlight, UpgradeDecisionEvent, UpgradeState};

/// The event a running batch reports its decks on.
pub(crate) const BATCH_EVENT: &str = "desktop://upgrade-all";

/// What the webview reads when it runs a batch that is not the last one
/// checked, or one that already ran.
pub(crate) const NOT_CHECKED: &str =
    "That check is no longer current. Open Upgrade all again to check every daemon afresh.";

/// What one check found: the decks, the plan, and each deck's wire id.
pub(crate) struct Checked {
    decks: Vec<RemoteBatchDeck>,
    plan: BatchPlan,
    /// The wire id of each deck the app shows for that row, by plan index.
    deck_ids: Vec<Option<String>>,
}

impl Checked {
    /// Read the deck list at `path` and check every deck on it at once.
    pub(crate) fn check(path: &Path) -> Result<Self, String> {
        let decks = RemoteBatchDeck::from_deck_list(path, no_report).map_err(safe_message)?;
        let refs: Vec<&dyn BatchDeck> = decks.iter().map(|d| d as &dyn BatchDeck).collect();
        let plan = check_all(&refs);
        let deck_ids = decks.iter().map(|deck| deck_id_of(deck.entry())).collect();
        Ok(Self {
            decks,
            plan,
            deck_ids,
        })
    }

    /// Claim the in-flight slot of every deck the run would upgrade, so a
    /// single Upgrade of one of them cannot start under the batch. Refused,
    /// naming the deck, while another upgrade holds one.
    pub(crate) fn reserve(&self, upgrades: &UpgradeState) -> Result<Vec<InFlight>, String> {
        let mut slots = Vec::new();
        for (planned, id) in self.plan.decks.iter().zip(&self.deck_ids) {
            if let (DeckCheck::Upgrade { .. }, Some(id)) = (&planned.check, id) {
                let slot = upgrades.begin(id).map_err(|_| {
                    format!(
                        "An upgrade of {} is already running in this app. Wait for it to finish, then press Upgrade all again.",
                        safe_message(&planned.deck)
                    )
                })?;
                slots.push(slot);
            }
        }
        Ok(slots)
    }

    pub(crate) fn decks(&self) -> Vec<&dyn BatchDeck> {
        self.decks.iter().map(|d| d as &dyn BatchDeck).collect()
    }

    pub(crate) fn plan(&self) -> &BatchPlan {
        &self.plan
    }

    pub(crate) fn deck_ids(&self) -> &[Option<String>] {
        &self.deck_ids
    }

    /// The wire id of each deck the plan names, by deck name: what a
    /// question's `deckId` says.
    fn ids_by_name(&self) -> HashMap<String, String> {
        self.plan
            .decks
            .iter()
            .zip(&self.deck_ids)
            .filter_map(|(planned, id)| Some((planned.deck.clone(), id.clone()?)))
            .collect()
    }
}

/// The wire id the app shows a deck-list row under, when it describes a deck.
fn deck_id_of(entry: &RemoteEntry) -> Option<String> {
    let remote = crate::decks::row_from_entry(entry).ok()?.endpoint()?;
    Some(crate::dto::deck_wire_id(&Endpoint::Remote(remote)))
}

/// The checked batches waiting for a run, and the runs that can be stopped.
#[derive(Default, Clone)]
pub(crate) struct UpgradeAllState {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The last check, by its batch id. A newer check replaces it, so an
    /// abandoned dialog leaves nothing behind.
    checked: Mutex<Option<(String, Checked)>>,
    /// The runs in progress, by batch id, with their stop flag.
    running: Mutex<HashMap<String, Arc<AtomicBool>>>,
    next_id: AtomicU64,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A run's place in [`UpgradeAllState`], released on drop.
pub(crate) struct Running {
    inner: Arc<Inner>,
    batch_id: String,
    stop: Arc<AtomicBool>,
}

impl Running {
    pub(crate) fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        lock(&self.inner.running).remove(&self.batch_id);
    }
}

impl UpgradeAllState {
    /// Keep `checked` for a run and describe it for the webview.
    pub(crate) fn insert(&self, checked: Checked) -> UpgradeAllPlanDto {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let batch_id = format!("batch-{n}");
        let dto = UpgradeAllPlanDto::new(&batch_id, &checked);
        *lock(&self.inner.checked) = Some((batch_id, checked));
        dto
    }

    /// Take the checked batch `batch_id` for its run. It runs once.
    pub(crate) fn take(&self, batch_id: &str) -> Result<Checked, String> {
        let mut checked = lock(&self.inner.checked);
        match checked.take() {
            Some((id, batch)) if id == batch_id => Ok(batch),
            other => {
                *checked = other;
                Err(NOT_CHECKED.to_string())
            }
        }
    }

    /// Register the run of `batch_id`, so it can be stopped.
    pub(crate) fn start(&self, batch_id: &str) -> Running {
        let stop = Arc::new(AtomicBool::new(false));
        lock(&self.inner.running).insert(batch_id.to_string(), Arc::clone(&stop));
        Running {
            inner: Arc::clone(&self.inner),
            batch_id: batch_id.to_string(),
            stop,
        }
    }

    /// Start no deck of `batch_id` after the one being upgraded.
    pub(crate) fn stop(&self, batch_id: &str) -> Result<(), String> {
        match lock(&self.inner.running).get(batch_id) {
            Some(stop) => {
                stop.store(true, Ordering::SeqCst);
                Ok(())
            }
            None => Err("That upgrade is no longer running.".to_string()),
        }
    }
}

/// Asks each daemon's restart question in the webview, through the same event
/// and answer a single Upgrade uses, naming the deck that asks.
pub(crate) struct BatchDecider {
    upgrades: UpgradeState,
    attempt_id: String,
    upgrade_id: String,
    deck_ids: HashMap<String, String>,
    emit: Arc<dyn Fn(&UpgradeDecisionEvent) + Send + Sync>,
}

impl BatchDecider {
    pub(crate) fn new(
        upgrades: UpgradeState,
        checked: &Checked,
        attempt_id: String,
        upgrade_id: String,
        emit: Arc<dyn Fn(&UpgradeDecisionEvent) + Send + Sync>,
    ) -> Self {
        Self {
            upgrades,
            attempt_id,
            upgrade_id,
            deck_ids: checked.ids_by_name(),
            emit,
        }
    }
}

impl RestartDecider for BatchDecider {
    fn decide(&self, deck: &str, at_stake: &RestartStopSet, stale: bool) -> RestartChoice {
        let emit = Arc::clone(&self.emit);
        DesktopDecider::new(
            self.upgrades.clone(),
            self.deck_ids.get(deck).cloned().unwrap_or_default(),
            self.attempt_id.clone(),
            self.upgrade_id.clone(),
            Box::new(move |event| emit(event)),
        )
        .decide(deck, at_stake, stale)
    }
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// What `desktop_upgrade_all_check` resolves with.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpgradeAllPlanDto {
    pub batch_id: String,
    /// "Checked 3 remote decks: 2 to upgrade.", the crate's words.
    pub headline: String,
    pub to_upgrade: usize,
    pub decks: Vec<PlannedDeckDto>,
}

/// How a deck reads in the plan.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PlannedKind {
    Upgrade,
    Skip,
    Unreachable,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlannedDeckDto {
    pub deck: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deck_id: Option<String>,
    pub kind: PlannedKind,
    pub line: String,
}

impl UpgradeAllPlanDto {
    fn new(batch_id: &str, checked: &Checked) -> Self {
        let plan = &checked.plan;
        Self {
            batch_id: batch_id.to_string(),
            headline: safe_message(plan.summary().lines().next().unwrap_or_default()),
            to_upgrade: plan.to_upgrade(),
            decks: plan
                .decks
                .iter()
                .zip(&checked.deck_ids)
                .map(|(planned, id)| PlannedDeckDto {
                    deck: safe_message(&planned.deck),
                    deck_id: id.clone(),
                    kind: match planned.check {
                        DeckCheck::Upgrade { .. } => PlannedKind::Upgrade,
                        DeckCheck::Skip { .. } => PlannedKind::Skip,
                        DeckCheck::Unreachable { .. } => PlannedKind::Unreachable,
                    },
                    line: safe_message(planned.line()),
                })
                .collect(),
        }
    }
}

/// One deck's result.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeckReportDto {
    pub deck: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deck_id: Option<String>,
    pub status: DeckStatus,
    pub line: String,
    /// Whether the deck's terminal sessions ended with its old daemon, as
    /// after a single Upgrade that restarted it.
    pub ends_sessions: bool,
}

impl DeckReportDto {
    pub(crate) fn new(report: &DeckReport, deck_id: Option<&String>) -> Self {
        Self {
            deck: safe_message(&report.deck),
            deck_id: deck_id.cloned(),
            status: report.status,
            line: safe_message(&report.line),
            ends_sessions: ends_sessions(report),
        }
    }
}

/// Whether `report`'s deck ran an upgrade whose old daemon may be gone.
pub(crate) fn ends_sessions(report: &DeckReport) -> bool {
    matches!(&report.result, DeckResult::Ran { outcome } if crate::upgrade::ends_deck_sessions(outcome))
}

/// What `desktop_upgrade_all_run` resolves with.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpgradeAllReportDto {
    /// "Upgrade all: 1 upgraded, 1 skipped of 2 remote decks.", the crate's words.
    pub headline: String,
    /// [`BatchReport::is_failure`]: a deck failed or could not be checked.
    pub failed: bool,
    pub decks: Vec<DeckReportDto>,
}

impl UpgradeAllReportDto {
    pub(crate) fn new(report: &BatchReport, deck_ids: &[Option<String>]) -> Self {
        Self {
            headline: safe_message(report.headline()),
            failed: report.is_failure(),
            decks: report
                .decks
                .iter()
                .enumerate()
                .map(|(i, deck)| DeckReportDto::new(deck, deck_ids.get(i).and_then(Option::as_ref)))
                .collect(),
        }
    }
}

/// `desktop://upgrade-all`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpgradeAllEventDto {
    pub batch_id: String,
    /// The id the webview chose for this run; its listener hears only these.
    pub attempt_id: String,
    pub index: usize,
    #[serde(flatten)]
    pub kind: UpgradeAllEventKind,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum UpgradeAllEventKind {
    Started,
    Progress { stage: UpgradeStage },
    Finished { report: DeckReportDto },
}

impl UpgradeAllEventDto {
    pub(crate) fn new(
        batch_id: &str,
        attempt_id: &str,
        deck_ids: &[Option<String>],
        event: &BatchEvent,
    ) -> Self {
        let (index, kind) = match event {
            BatchEvent::Started { index, .. } => (*index, UpgradeAllEventKind::Started),
            BatchEvent::Progress {
                index, progress, ..
            } => (
                *index,
                UpgradeAllEventKind::Progress {
                    stage: progress.stage,
                },
            ),
            BatchEvent::Finished { index, report } => (
                *index,
                UpgradeAllEventKind::Finished {
                    report: DeckReportDto::new(
                        report,
                        deck_ids.get(*index).and_then(Option::as_ref),
                    ),
                },
            ),
        };
        Self {
            batch_id: batch_id.to_string(),
            attempt_id: attempt_id.to_string(),
            index,
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::daemon_upgrade::UpgradeOutcome;
    use dot_agent_deck::daemon_upgrade_batch::{PlannedDeck, SkipReason};

    fn checked(decks: Vec<(&str, DeckCheck, Option<&str>)>) -> Checked {
        Checked {
            decks: Vec::new(),
            plan: BatchPlan {
                decks: decks
                    .iter()
                    .map(|(deck, check, _)| PlannedDeck {
                        deck: deck.to_string(),
                        check: check.clone(),
                    })
                    .collect(),
            },
            deck_ids: decks
                .iter()
                .map(|(_, _, id)| id.map(str::to_string))
                .collect(),
        }
    }

    fn older() -> DeckCheck {
        DeckCheck::Upgrade {
            from: "0.51.0".into(),
            to: "0.52.0".into(),
        }
    }

    #[test]
    fn a_batch_runs_once_and_only_the_last_check_runs() {
        let state = UpgradeAllState::default();
        let first = state.insert(checked(vec![("alpha", older(), Some("deck-a"))]));
        let second = state.insert(checked(vec![("alpha", older(), Some("deck-a"))]));
        assert_eq!(
            state.take(&first.batch_id).err().as_deref(),
            Some(NOT_CHECKED)
        );
        assert!(state.take(&second.batch_id).is_ok());
        assert_eq!(
            state.take(&second.batch_id).err().as_deref(),
            Some(NOT_CHECKED),
            "a batch runs once"
        );
    }

    #[test]
    fn a_run_is_stopped_by_its_id_and_forgotten_when_it_ends() {
        let state = UpgradeAllState::default();
        let running = state.start("batch-7");
        assert!(state.stop("batch-7").is_ok());
        assert!(running.stop_flag().load(Ordering::SeqCst));
        drop(running);
        assert!(state.stop("batch-7").is_err());
    }

    #[test]
    fn the_decks_to_upgrade_are_reserved_and_a_busy_one_refuses_the_batch() {
        let upgrades = UpgradeState::default();
        let batch = checked(vec![
            ("alpha", older(), Some("deck-a")),
            (
                "beta",
                DeckCheck::Skip {
                    reason: SkipReason::NoDaemonRunning,
                },
                Some("deck-b"),
            ),
        ]);
        let slots = batch.reserve(&upgrades).unwrap();
        assert_eq!(slots.len(), 1, "only a deck to upgrade is reserved");
        assert!(upgrades.begin("deck-a").is_err(), "a single Upgrade waits");
        assert!(
            upgrades.begin("deck-b").is_ok(),
            "a skipped deck is not held"
        );
        let refused = batch.reserve(&upgrades).err().unwrap();
        assert!(refused.contains("alpha"), "{refused}");
        drop(slots);
        assert!(upgrades.begin("deck-a").is_ok());
    }

    #[test]
    fn the_plan_and_report_carry_the_crates_words_in_camel_case() {
        let batch = checked(vec![("alpha", older(), Some("deck-a"))]);
        let plan = UpgradeAllPlanDto::new("batch-1", &batch);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["batchId"], "batch-1");
        assert_eq!(json["toUpgrade"], 1);
        assert_eq!(json["headline"], "Checked 1 remote deck: 1 to upgrade.");
        assert_eq!(json["decks"][0]["deckId"], "deck-a");
        assert_eq!(json["decks"][0]["kind"], "upgrade");
        assert_eq!(
            json["decks"][0]["line"],
            "Will be upgraded from 0.51.0 to 0.52.0."
        );

        let report = BatchReport {
            decks: vec![DeckReport::new(
                "alpha",
                DeckResult::Ran {
                    outcome: UpgradeOutcome::Restarted {
                        from_version: "0.51.0".into(),
                        to_version: "0.52.0".into(),
                        stopped: RestartStopSet::default(),
                    },
                },
            )],
        };
        let json =
            serde_json::to_value(UpgradeAllReportDto::new(&report, batch.deck_ids())).unwrap();
        assert_eq!(json["failed"], false);
        assert_eq!(json["decks"][0]["status"], "upgraded");
        assert_eq!(json["decks"][0]["endsSessions"], true);
        assert_eq!(
            json["decks"][0]["line"],
            "Upgraded: now runs 0.52.0 (was 0.51.0)."
        );

        let event = UpgradeAllEventDto::new(
            "batch-1",
            "attempt-1",
            batch.deck_ids(),
            &BatchEvent::Finished {
                index: 0,
                report: report.decks[0].clone(),
            },
        );
        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["type"], "finished");
        assert_eq!(json["attemptId"], "attempt-1");
        assert_eq!(json["report"]["deckId"], "deck-a");
    }

    #[test]
    fn a_question_names_the_deck_that_asks() {
        let upgrades = UpgradeState::default();
        let batch = checked(vec![
            ("alpha", older(), Some("deck-a")),
            ("beta", older(), Some("deck-b")),
        ]);
        let asked: Arc<Mutex<Vec<UpgradeDecisionEvent>>> = Arc::default();
        let seen = Arc::clone(&asked);
        let answering = upgrades.clone();
        let decider = BatchDecider::new(
            upgrades.clone(),
            &batch,
            "attempt-1".into(),
            "upgrade-9".into(),
            Arc::new(move |event: &UpgradeDecisionEvent| {
                seen.lock().unwrap().push(event.clone());
                // Answer at once, as a dialog would.
                let _ = answering.decide(
                    &event.upgrade_id,
                    event.question_id,
                    crate::upgrade::DecisionChoice::RestartNow,
                );
            }),
        );
        assert_eq!(
            decider.decide("beta", &RestartStopSet::default(), false),
            RestartChoice::RestartNow
        );
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].deck_id, "deck-b");
        assert_eq!(asked[0].attempt_id, "attempt-1");
    }
}
