//! PRD #1666 part 3, round 5 (plan §6.1, §6.2, §6.6): a worker's task-scoped
//! `work-done` through the real transaction — the registry, the hub and its
//! task ledger, the retained store — with `cat` stand-ins for the
//! orchestrator and its workers. Delegates are posted through the hub with a
//! real commission arm, and reports go through
//! [`crate::task_completion::complete_with_state`], attested as the
//! generation that sends them.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use spec::spec;

use super::dispatch_lifecycle_tests::{Deck, deck, eventually, kill_child, rows, spawn_cat};
use super::*;
use crate::event::{WorkDoneDisposition, WorkDoneMode, WorkDoneTaskReply};
use crate::hook_provenance::{AttestFacts, Provenance, Seat};
use crate::inbox::{Content, Recipient, StateName};
use crate::report_recovery::{Destination, OutcomeCause, OutcomePhase};
use crate::state::{AppState, OrchestrationIdentity};
use crate::task_completion::{Completion, CompletionRequest};
use crate::work_done_text::code;

/// An orchestration's two panes and the role map the daemon keeps for them.
pub(super) struct Team {
    pub(super) deck: Deck,
    pub(super) state: AppState,
    pub(super) orch_pane: String,
    pub(super) orch: String,
    pub(super) worker_pane: String,
    pub(super) worker: String,
    pub(super) instance: String,
    /// Keeps the role map's working directory for the team's life.
    pub(super) _cwd: tempfile::TempDir,
}

pub(super) fn team(tag: &str) -> Team {
    team_on(deck(), tag)
}

pub(super) fn team_on(deck: Deck, tag: &str) -> Team {
    let orch_pane = format!("{tag}-orch");
    let worker_pane = format!("{tag}-coder");
    let orch = spawn_cat(&deck.registry, &orch_pane);
    let worker = spawn_cat(&deck.registry, &worker_pane);
    let cwd = crate::test_temp::tempdir().unwrap();
    let cwd_str = cwd.path().to_string_lossy().into_owned();
    let instance = format!("{tag}-instance");
    let identity = OrchestrationIdentity {
        id: instance.clone(),
        name: "team".to_string(),
    };
    let mut state = AppState::default();
    for (pane, role) in [(&orch_pane, "orchestrator"), (&worker_pane, "coder")] {
        state.pane_role_map.insert(pane.clone(), role.to_string());
        state
            .pane_orchestration_map
            .insert(pane.clone(), identity.clone());
        state.pane_cwd_map.insert(pane.clone(), cwd_str.clone());
    }
    state.orchestrator_pane_ids.insert(orch_pane.clone());
    Team {
        deck,
        state,
        orch_pane,
        orch,
        worker_pane,
        worker,
        instance,
        _cwd: cwd,
    }
}

/// What a report may change, read whole, for "nothing changed" assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Effects {
    arms: Vec<u64>,
    watches: Vec<String>,
    open_tasks: Vec<String>,
    orch_inbox: Vec<(String, StateName)>,
    rows: usize,
    files: Vec<String>,
    claims: usize,
}

impl Team {
    pub(super) fn registry(&self) -> &Arc<AgentPtyRegistry> {
        &self.deck.registry
    }

    /// Delegate a task to the worker, with a real commission arm, as the
    /// daemon's delegate path posts it. Answers its id.
    pub(super) fn delegate(&self, text: &str) -> String {
        self.delegate_to(&self.worker_pane, &self.worker, text)
    }

    pub(super) fn delegate_to(&self, pane: &str, agent: &str, text: &str) -> String {
        let arm = match self.registry().arm_delegation_commission(
            pane,
            &self.orch_pane,
            Some(&self.orch),
            true,
        ) {
            CommissionArm::Armed { in_flight, .. } => Some(in_flight.arm_id()),
            _ => None,
        };
        let posted = self
            .registry()
            .inbox()
            .post_delegate(
                Recipient::bound(pane, agent),
                "coder",
                Some(Recipient::bound(&self.orch_pane, &self.orch)),
                crate::inbox_hub::DelegateLedger {
                    commission_arm_id: arm,
                    delegation_seq: None,
                    instance: Some(self.instance.clone()),
                },
                Content::from_deck("delegate", "task", text),
                &|| true,
            )
            .expect("delegated");
        let (superseded, task) = (
            self.registry().inbox().open_tasks_of(pane),
            posted.0.to_string(),
        );
        // What the daemon's delegate path releases for the tasks this one
        // superseded: its own arm stays, the older tasks' go.
        let older: Vec<u64> = self
            .registry()
            .commission_arm_ids_for_test(pane)
            .into_iter()
            .filter(|id| Some(*id) != arm)
            .collect();
        if !superseded.iter().any(|id| *id != task) {
            let debt: Vec<_> = older
                .into_iter()
                .map(|id| crate::inbox_hub::DelegateLedger {
                    commission_arm_id: Some(id),
                    ..Default::default()
                })
                .collect();
            self.registry().retire_task_debt_exact(pane, &debt);
        }
        task
    }

    /// The worker takes and acknowledges what waits for it.
    pub(super) fn acknowledge(&self) {
        self.registry()
            .inbox()
            .test_take_acked(&self.worker_pane, &self.worker);
    }

    pub(super) fn attested(&self, agent: &str) -> Provenance {
        Provenance::Attested {
            agent_id: agent.to_string(),
            facts: self.registry().attest_facts(agent).unwrap_or_default(),
        }
    }

    /// The worker reports `mode` with `body`, attested as itself.
    pub(super) async fn report(&self, mode: WorkDoneMode, body: &str) -> Completion {
        self.report_as(&self.worker_pane, &self.attested(&self.worker), mode, body)
            .await
    }

    pub(super) async fn report_as(
        &self,
        pane: &str,
        provenance: &Provenance,
        mode: WorkDoneMode,
        body: &str,
    ) -> Completion {
        crate::task_completion::complete_with_state(
            &self.state,
            self.registry(),
            CompletionRequest {
                pane_id: pane.to_string(),
                mode,
                report: body.to_string(),
            },
            provenance,
        )
        .await
    }

    pub(super) async fn report_task(&self, task: &str, body: &str) -> Completion {
        self.report(
            WorkDoneMode::Task {
                task_id: task.to_string(),
            },
            body,
        )
        .await
    }

    pub(super) fn effects(&self) -> Effects {
        let mut files: Vec<String> = std::fs::read_dir(self.deck.store.dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|file| file.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        Effects {
            arms: self
                .registry()
                .commission_arm_ids_for_test(&self.worker_pane),
            watches: self.registry().inbox().watched_tasks_for_test(),
            open_tasks: self.registry().inbox().open_tasks_of(&self.worker_pane),
            orch_inbox: self
                .registry()
                .inbox()
                .list(&self.orch_pane)
                .into_iter()
                .map(|m| (m.subject, m.state))
                .collect(),
            rows: rows(&self.deck).len(),
            files,
            claims: self
                .registry()
                .inbox()
                .read_reports(|r| r.claims.len())
                .unwrap_or(usize::MAX),
        }
    }

    /// The orchestrator's inbox, subjects and states.
    pub(super) fn orch_subjects(&self) -> Vec<String> {
        self.registry()
            .inbox()
            .list(&self.orch_pane)
            .into_iter()
            .map(|m| m.subject)
            .collect()
    }
}

pub(super) fn reply(completion: Completion) -> WorkDoneTaskReply {
    match completion {
        Completion::Reply(reply) => reply,
        Completion::Unconfirmed => panic!("expected an answer, got unconfirmed"),
    }
}

pub(super) fn refused_with(completion: Completion, expected: &str) -> WorkDoneTaskReply {
    let reply = reply(completion);
    assert_eq!(reply.disposition, WorkDoneDisposition::Refused, "{reply:?}");
    assert_eq!(reply.code.as_deref(), Some(expected), "{reply:?}");
    reply
}

fn accepted(completion: Completion) -> WorkDoneTaskReply {
    let reply = reply(completion);
    assert_eq!(
        reply.disposition,
        WorkDoneDisposition::Accepted,
        "{reply:?}"
    );
    reply
}

/// Scenario: A worker reports task t1, which is accepted; the orchestrator then delegates t2. The same t1 report sent again is answered with t1's first result — the same report id — and changes nothing: t2 stays open, its arm, its watch and the orchestrator's inbox are as they were.
#[spec("orchestration/work-done/016")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_016_a_repeat_of_t1_after_t2_is_answered_from_t1() {
    let team = team("wd016");
    let t1 = team.delegate("first task");
    team.acknowledge();
    let first = accepted(team.report_task(&t1, "t1 done").await);
    assert_eq!(first.destination, Some(Destination::Inbox));
    let t2 = team.delegate("second task");
    let before = team.effects();
    assert_eq!(before.open_tasks, vec![t2.clone()]);
    let again = reply(team.report_task(&t1, "t1 done").await);
    assert_eq!(again.disposition, WorkDoneDisposition::AlreadyAccepted);
    assert_eq!(again.report_id, first.report_id, "the first result");
    assert!(!again.differs);
    assert_eq!(team.effects(), before, "a repeat changes nothing");
}

/// Scenario: A worker with an open task reports with no task id. The orchestrator gets a note that says it completes no task; the task stays open and its arm, watch and record are untouched, so the worker still owes the work-done for it.
#[spec("orchestration/work-done/017")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_017_a_report_with_no_task_id_completes_nothing() {
    let team = team("wd017");
    let task = team.delegate("a task");
    let before = team.effects();
    let note = reply(team.report(WorkDoneMode::Uncorrelated, "a note").await);
    assert_eq!(note.disposition, WorkDoneDisposition::Uncorrelated);
    assert_eq!(note.destination, Some(Destination::Inbox));
    let after = team.effects();
    assert_eq!(after.open_tasks, vec![task]);
    assert_eq!(after.arms, before.arms);
    assert_eq!(after.watches, before.watches);
    assert!(
        team.registry()
            .owes_delegation_commission(&team.worker_pane)
    );
    assert!(
        team.orch_subjects().iter().any(|s| s == "note from coder"),
        "{:?}",
        team.orch_subjects()
    );
}

/// Scenario: Task t is delegated to worker w1. A report naming t from another worker pane, from another generation of w1's pane, and from w1 attested into another orchestration instance is each refused as an unknown task, and changes nothing.
#[spec("orchestration/work-done/018")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_018_another_worker_generation_or_orchestration_is_refused() {
    let team = team("wd018");
    let task = team.delegate("w1's task");
    let other_pane = "wd018-other";
    let other = spawn_cat(team.registry(), other_pane);
    let before = team.effects();
    let mode = || WorkDoneMode::Task {
        task_id: task.clone(),
    };
    refused_with(
        team.report_as(other_pane, &team.attested(&other), mode(), "x")
            .await,
        code::UNKNOWN_TASK,
    );
    refused_with(
        team.report_as(
            &team.worker_pane,
            &Provenance::Attested {
                agent_id: "another-generation".to_string(),
                facts: AttestFacts::default(),
            },
            mode(),
            "x",
        )
        .await,
        code::UNKNOWN_TASK,
    );
    refused_with(
        team.report_as(
            &team.worker_pane,
            &Provenance::Attested {
                agent_id: team.worker.clone(),
                facts: AttestFacts {
                    seat: Some(Seat {
                        instance: "another-instance".to_string(),
                        is_orchestrator: false,
                        origin: None,
                    }),
                    unit: None,
                },
            },
            mode(),
            "x",
        )
        .await,
        code::UNKNOWN_TASK,
    );
    assert_eq!(team.effects(), before);
}

/// Scenario: A report names a task id the deck never minted, a malformed id, and a task whose tombstone the bound evicted. Each is refused as unknown, and changes nothing; the evicted one is never rebound.
#[spec("orchestration/work-done/019")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_019_an_unknown_or_forgotten_task_is_refused() {
    let team = team("wd019");
    team.registry().inbox().set_task_tombstones_for_test(1);
    let old = team.delegate("old task");
    accepted(team.report_task(&old, "old done").await);
    let newer = team.delegate("newer task");
    accepted(team.report_task(&newer, "newer done").await);
    let before = team.effects();
    refused_with(
        team.report_task("m-aaaaaaaa-0000000000000000", "x").await,
        code::UNKNOWN_TASK,
    );
    refused_with(team.report_task("not an id", "x").await, code::UNKNOWN_TASK);
    let forgotten = refused_with(team.report_task(&old, "old done").await, code::UNKNOWN_TASK);
    assert!(
        forgotten
            .message
            .as_deref()
            .is_some_and(|m| m.contains("does not know task")),
        "{forgotten:?}"
    );
    assert_eq!(team.effects(), before);
}

/// Scenario: A worker reports its task twice with the same body. The second is answered with the first's report id and destination, exit 0, and nothing new is posted, saved or retired.
#[spec("orchestration/work-done/020")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_020_a_double_report_is_answered_with_the_first() {
    let team = team("wd020");
    let task = team.delegate("a task");
    let first = accepted(team.report_task(&task, "done").await);
    let before = team.effects();
    let again = reply(team.report_task(&task, "done").await);
    assert_eq!(again.disposition, WorkDoneDisposition::AlreadyAccepted);
    assert_eq!(
        (again.report_id, again.destination, again.differs),
        (first.report_id, first.destination, false)
    );
    assert_eq!(team.effects(), before);
    let answer = crate::work_done_text::cli_answer(
        &reply(team.report_task(&task, "done").await),
        Some(&task),
        false,
    );
    assert!(answer.success, "exit 0");
}

/// Scenario: A worker reports its task, then sends a different report for the same task. The repeat is answered `differs`, exit 1, nothing is delivered, and the first report stands in the orchestrator's inbox.
#[spec("orchestration/work-done/021")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_021_a_differing_duplicate_is_not_delivered() {
    let team = team("wd021");
    let task = team.delegate("a task");
    accepted(team.report_task(&task, "the first body").await);
    let before = team.effects();
    let differs = reply(team.report_task(&task, "another body").await);
    assert_eq!(differs.disposition, WorkDoneDisposition::AlreadyAccepted);
    assert!(differs.differs);
    let answer = crate::work_done_text::cli_answer(&differs, Some(&task), false);
    assert!(!answer.success, "exit 1");
    assert!(answer.stderr.unwrap().contains("NOT delivered"));
    assert_eq!(team.effects(), before);
}

/// Scenario: Two workers each have a task. One worker's accepted report retires exactly its own task — its arm, its watch and its open entry — and the other worker's arm, watch and task are untouched; the orchestrator gets one report naming the task id.
#[spec("orchestration/work-done/022")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_022_an_accepted_report_retires_exactly_its_own_task() {
    let team = team("wd022");
    let mine = team.delegate("mine");
    let sibling_pane = "wd022-sibling";
    let sibling = spawn_cat(team.registry(), sibling_pane);
    let theirs = team.delegate_to(sibling_pane, &sibling, "theirs");
    let sibling_arms = team.registry().commission_arm_ids_for_test(sibling_pane);
    assert!(team.effects().watches.contains(&mine));
    let done = accepted(team.report_task(&mine, "mine done").await);
    assert_eq!(done.task_id.as_deref(), Some(mine.as_str()));
    let after = team.effects();
    assert!(after.arms.is_empty(), "its own arm went");
    assert!(after.open_tasks.is_empty());
    assert!(!after.watches.contains(&mine));
    assert!(after.watches.contains(&theirs));
    assert_eq!(
        team.registry().commission_arm_ids_for_test(sibling_pane),
        sibling_arms
    );
    assert_eq!(
        team.registry().inbox().open_tasks_of(sibling_pane),
        vec![theirs]
    );
    assert_eq!(
        team.orch_subjects(),
        vec![format!("coder completed task {mine}")]
    );
}

/// Scenario: A task superseded by a newer delegate is reported. The report is refused, naming the task that replaced it, and changes nothing.
#[spec("orchestration/work-done/023")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_023_a_superseded_task_is_refused_naming_its_successor() {
    let team = team("wd023");
    let old = team.delegate("old");
    let new = team.delegate("new");
    let before = team.effects();
    let refusal = refused_with(
        team.report_task(&old, "old done").await,
        code::TASK_SUPERSEDED,
    );
    assert!(refusal.message.unwrap().contains(&new));
    assert_eq!(team.effects(), before);
}

/// Scenario: A task report and a terminal report arrive with no hook capability token attesting them. Both are refused as unattested before anything is read, and nothing changes.
#[spec("orchestration/work-done/024")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_024_unattested_task_and_terminal_reports_are_refused() {
    let team = team("wd024");
    let task = team.delegate("a task");
    let before = team.effects();
    refused_with(
        team.report_as(
            &team.worker_pane,
            &Provenance::Unattested,
            WorkDoneMode::Task { task_id: task },
            "x",
        )
        .await,
        code::UNATTESTED,
    );
    refused_with(
        team.report_as(
            &team.worker_pane,
            &Provenance::Unattested,
            WorkDoneMode::Terminal,
            "x",
        )
        .await,
        code::UNATTESTED,
    );
    assert_eq!(team.effects(), before);
}

/// Scenario: The orchestrator is replaced before its worker reports two tasks. Each report is kept as its own recovery row whose saved file carries the envelope and the report; the replacement orchestrator gets nothing.
#[spec("orchestration/work-done/025")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_025_a_replaced_orchestrator_leaves_the_report_in_recovery() {
    let team = team("wd025");
    let t1 = team.delegate("t1");
    kill_child(team.registry(), &team.orch);
    eventually("the orchestrator exits", || {
        team.registry()
            .pane_current_agent_id(&team.orch_pane)
            .is_none()
    })
    .await;
    let successor = spawn_cat(team.registry(), &team.orch_pane);
    let first = accepted(team.report_task(&t1, "SENTINEL-wd025-a").await);
    assert_eq!(first.destination, Some(Destination::Recovery));
    let t2 = team.delegate_to(&team.worker_pane, &team.worker, "t2");
    let second = accepted(team.report_task(&t2, "SENTINEL-wd025-b").await);
    assert_eq!(second.destination, Some(Destination::Recovery));
    let kept: Vec<_> = rows(&team.deck)
        .into_iter()
        .filter(|row| row.phase == OutcomePhase::WorkerReport)
        .collect();
    assert_eq!(kept.len(), 2, "{kept:?}");
    for (row, sentinel) in kept.iter().zip(["SENTINEL-wd025-a", "SENTINEL-wd025-b"]) {
        assert_eq!(row.cause, OutcomeCause::RecipientExited);
        let file = team
            .deck
            .store
            .dir()
            .join(format!("r-{}.md", row.report_id.as_deref().unwrap()));
        let text = std::fs::read_to_string(file).unwrap();
        assert!(text.contains(crate::retained_store::MAGIC));
        assert!(text.contains(sentinel), "{text}");
    }
    assert!(
        team.registry()
            .inbox()
            .list(&team.orch_pane)
            .iter()
            .all(|m| m.recipient.as_ref().and_then(|r| r.agent_id.clone())
                != Some(successor.clone())),
        "the successor gets nothing"
    );
}

/// Scenario: A report's save fails (an injected write failure). The worker reads `report_save_failed` and nothing changes; the same report sent again is accepted.
#[spec("orchestration/work-done/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_026_a_failed_save_changes_nothing_and_a_retry_is_accepted() {
    let team = team("wd026");
    let task = team.delegate("a task");
    let before = team.effects();
    team.registry()
        .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
    refused_with(
        team.report_task(&task, "done").await,
        code::REPORT_SAVE_FAILED,
    );
    assert_eq!(team.effects(), before);
    accepted(team.report_task(&task, "done").await);
}

/// Scenario: The orchestrator exits while its worker's report is paused after its save. Released, the report is kept in recovery, never delivered to the orchestrator's successor.
#[spec("orchestration/work-done/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_027_the_orchestrator_exiting_during_the_save_sends_it_to_recovery() {
    let team = Arc::new(team("wd027"));
    let task = team.delegate("a task");
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
    let report = {
        let team = Arc::clone(&team);
        let task = task.clone();
        tokio::spawn(async move { team.report_task(&task, "done").await })
    };
    reached.await.unwrap();
    kill_child(team.registry(), &team.orch);
    eventually("the orchestrator exits", || {
        team.registry()
            .pane_current_agent_id(&team.orch_pane)
            .is_none()
    })
    .await;
    let _successor = spawn_cat(team.registry(), &team.orch_pane);
    release.send(()).unwrap();
    let done = accepted(report.await.unwrap());
    assert_eq!(done.destination, Some(Destination::Recovery));
    assert!(
        rows(&team.deck)
            .iter()
            .any(|row| row.task_id.as_deref() == Some(task.as_str()))
    );
}

/// Scenario: A report is delivered to the orchestrator's inbox, and the orchestrator exits before taking it: the report becomes a recovery row marked never handed. A report the orchestrator acknowledged leaves no row.
#[spec("orchestration/work-done/028")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_028_an_unconsumed_report_becomes_a_row_when_its_recipient_exits() {
    let acked = team("wd028a");
    let task = acked.delegate("a task");
    accepted(acked.report_task(&task, "done").await);
    acked
        .registry()
        .inbox()
        .test_take_acked(&acked.orch_pane, &acked.orch);
    kill_child(acked.registry(), &acked.orch);
    eventually("the orchestrator exits", || {
        acked
            .registry()
            .pane_current_agent_id(&acked.orch_pane)
            .is_none()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        rows(&acked.deck).is_empty(),
        "an acknowledged report leaves no row"
    );

    let team = team("wd028b");
    let task = team.delegate("a task");
    accepted(team.report_task(&task, "done").await);
    kill_child(team.registry(), &team.orch);
    eventually("one row", || {
        rows(&team.deck)
            .iter()
            .any(|row| row.task_id.as_deref() == Some(task.as_str()))
    })
    .await;
    let row = rows(&team.deck).into_iter().next().unwrap();
    assert_eq!(row.cause, OutcomeCause::RecipientExited);
    assert_eq!(
        row.delivery,
        crate::report_recovery::OutcomeDelivery::NeverHanded
    );
}

/// Scenario: With the retained store's file count at one and that slot taken, a report is refused `report_store_full` before anything is consumed; a report over the size limit is refused `report_too_large`.
#[spec("orchestration/work-done/029")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_029_a_full_store_and_an_oversized_report_are_refused() {
    let team = team_on(
        super::dispatch_lifecycle_tests::deck_with(crate::retained_store::StoreConfig {
            max_files: 1,
            ..Default::default()
        }),
        "wd029",
    );
    let t1 = team.delegate("t1");
    accepted(team.report_task(&t1, "done").await);
    let t2 = team.delegate("t2");
    let before = team.effects();
    refused_with(team.report_task(&t2, "done").await, code::REPORT_STORE_FULL);
    assert_eq!(team.effects(), before);
    let big = "x".repeat(crate::bounded_read::MAX_TASK_BYTES as usize + 1);
    refused_with(team.report_task(&t2, &big).await, code::REPORT_TOO_LARGE);
}

/// Scenario: t1's report is paused after its save; the orchestrator supersedes t1 with t2 meanwhile, which arms t2 beside it. Released, t1 commits: its own arm is released once, and t2's arm and open task are untouched.
#[spec("orchestration/work-done/041")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_041_a_claimed_task_is_not_superseded() {
    let team = Arc::new(team("wd041"));
    let t1 = team.delegate("t1");
    let t1_arms = team
        .registry()
        .commission_arm_ids_for_test(&team.worker_pane);
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
    let report = {
        let team = Arc::clone(&team);
        let t1 = t1.clone();
        tokio::spawn(async move { team.report_task(&t1, "t1 done").await })
    };
    reached.await.unwrap();
    let t2 = team.delegate("t2");
    let both = team
        .registry()
        .commission_arm_ids_for_test(&team.worker_pane);
    assert_eq!(both.len(), 2, "t2 is armed beside the claimed t1");
    release.send(()).unwrap();
    accepted(report.await.unwrap());
    let left = team
        .registry()
        .commission_arm_ids_for_test(&team.worker_pane);
    assert_eq!(left.len(), 1);
    assert!(!t1_arms.contains(&left[0]), "t1's arm went, t2's stayed");
    assert_eq!(
        team.registry().inbox().open_tasks_of(&team.worker_pane),
        vec![t2]
    );
}

/// Scenario: t1 is superseded by t2 before its report arrives. The report is refused `task_superseded` and changes nothing.
#[spec("orchestration/work-done/042")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_042_supersede_first_then_report_is_refused() {
    let team = team("wd042");
    let t1 = team.delegate("t1");
    let _t2 = team.delegate("t2");
    let before = team.effects();
    refused_with(
        team.report_task(&t1, "t1 done").await,
        code::TASK_SUPERSEDED,
    );
    assert_eq!(team.effects(), before);
}

/// Scenario: A worker's report is paused after its save, and the worker exits meanwhile. Released, the report is accepted; the orchestrator gets no "exited without work-done" notice and no undeliverable notice, and the task's debt is released once.
#[spec("orchestration/work-done/043")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_043_a_claim_before_the_worker_exits_commits_with_no_exit_notice() {
    let team = Arc::new(team("wd043"));
    let task = team.delegate("a task");
    team.acknowledge();
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
    let report = {
        let team = Arc::clone(&team);
        let task = task.clone();
        tokio::spawn(async move { team.report_task(&task, "done").await })
    };
    reached.await.unwrap();
    kill_child(team.registry(), &team.worker);
    eventually("the worker's exit is walked", || {
        team.registry()
            .commission_arm_ids_for_test(&team.worker_pane)
            .is_empty()
    })
    .await;
    release.send(()).unwrap();
    accepted(report.await.unwrap());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let subjects = team.orch_subjects();
    assert_eq!(
        subjects,
        vec![format!("coder completed task {task}")],
        "{subjects:?}"
    );
}

/// Scenario: A worker that acknowledged its task exits, and then its report arrives (its record and token remain). The report is refused `task_closed`, and the orchestrator was told of the exit exactly once.
#[spec("orchestration/work-done/044")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_044_a_report_after_the_worker_exited_is_refused() {
    let team = team("wd044");
    let task = team.delegate("a task");
    team.acknowledge();
    kill_child(team.registry(), &team.worker);
    eventually("the exit notice", || {
        team.orch_subjects().iter().any(|s| s == "worker exited")
    })
    .await;
    refused_with(team.report_task(&task, "done").await, code::TASK_CLOSED);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        team.orch_subjects()
            .iter()
            .filter(|s| *s == "worker exited")
            .count(),
        1
    );
}

/// Scenario: The first report of a task fails its save while a duplicate waits on it: the duplicate then proceeds fresh and is accepted. A duplicate waiting on a report that succeeds is answered `already_accepted`. A duplicate whose wait times out (a lowered knob) is answered `report_pending` and changes nothing.
#[spec("orchestration/work-done/046")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_046_a_waiting_duplicate_follows_the_first_attempt() {
    for fail_first in [true, false] {
        let team = Arc::new(team(&format!("wd046-{fail_first}")));
        let task = team.delegate("a task");
        let (reached, release) = team
            .registry()
            .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
        if fail_first {
            team.registry()
                .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
        }
        let first = {
            let team = Arc::clone(&team);
            let task = task.clone();
            tokio::spawn(async move { team.report_task(&task, "done").await })
        };
        reached.await.unwrap();
        let second = {
            let team = Arc::clone(&team);
            let task = task.clone();
            tokio::spawn(async move { team.report_task(&task, "done").await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        release.send(()).unwrap();
        let first = reply(first.await.unwrap());
        let second = reply(second.await.unwrap());
        if fail_first {
            assert_eq!(first.code.as_deref(), Some(code::REPORT_SAVE_FAILED));
            assert_eq!(second.disposition, WorkDoneDisposition::Accepted);
        } else {
            assert_eq!(first.disposition, WorkDoneDisposition::Accepted);
            assert_eq!(second.disposition, WorkDoneDisposition::AlreadyAccepted);
            assert_eq!(second.report_id, first.report_id);
        }
    }
}

/// Scenario: t1's report is paused while t2 is armed beside it; a report of t2 from the same generation is refused `report_pending`, since one generation holds one claim, and changes nothing.
#[spec("orchestration/work-done/047")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_047_one_generation_holds_one_claim() {
    let team = Arc::new(team("wd047"));
    let t1 = team.delegate("t1");
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
    let first = {
        let team = Arc::clone(&team);
        let t1 = t1.clone();
        tokio::spawn(async move { team.report_task(&t1, "t1 done").await })
    };
    reached.await.unwrap();
    let t2 = team.delegate("t2");
    let before = team.effects();
    refused_with(team.report_task(&t2, "t2 done").await, code::REPORT_PENDING);
    assert_eq!(team.effects(), before);
    release.send(()).unwrap();
    accepted(first.await.unwrap());
}

/// Scenario: A note is paused after its save and its orchestrator is replaced meanwhile: released, it is kept as a recovery row and the successor gets nothing. A note whose worker's pane now belongs to another orchestration instance is refused `not_a_worker`, and nothing is sent.
#[spec("orchestration/work-done/048")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_048_a_note_never_reaches_a_successor_or_another_orchestration() {
    let team = Arc::new(team("wd048"));
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
    let note = {
        let team = Arc::clone(&team);
        tokio::spawn(async move { team.report(WorkDoneMode::Uncorrelated, "a note").await })
    };
    reached.await.unwrap();
    kill_child(team.registry(), &team.orch);
    eventually("the orchestrator exits", || {
        team.registry()
            .pane_current_agent_id(&team.orch_pane)
            .is_none()
    })
    .await;
    let successor = spawn_cat(team.registry(), &team.orch_pane);
    release.send(()).unwrap();
    let kept = reply(note.await.unwrap());
    assert_eq!(kept.disposition, WorkDoneDisposition::Uncorrelated);
    assert_eq!(kept.destination, Some(Destination::Recovery));
    assert!(
        team.registry()
            .inbox()
            .list(&team.orch_pane)
            .iter()
            .all(|m| m.recipient.as_ref().and_then(|r| r.agent_id.clone())
                != Some(successor.clone()))
    );

    // (c) the pane now belongs to another orchestration instance.
    let other = super::report_tests::team("wd048c");
    let refused = other
        .report_as(
            &other.worker_pane,
            &Provenance::Attested {
                agent_id: other.worker.clone(),
                facts: AttestFacts {
                    seat: Some(Seat {
                        instance: "an-earlier-instance".to_string(),
                        is_orchestrator: false,
                        origin: None,
                    }),
                    unit: None,
                },
            },
            WorkDoneMode::Uncorrelated,
            "a note",
        )
        .await;
    refused_with(refused, code::NOT_A_WORKER);
    assert!(other.orch_subjects().is_empty());
}

/// Scenario: The reporting connection goes away while the save is paused (its future is dropped). Released, the claim still settles: the report is accepted, and a later repeat is answered from it.
#[spec("orchestration/work-done/052")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_052_a_save_outlives_its_connection() {
    let team = Arc::new(team("wd052"));
    let task = team.delegate("a task");
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
    let connection = {
        let team = Arc::clone(&team);
        let task = task.clone();
        tokio::spawn(async move { team.report_task(&task, "done").await })
    };
    reached.await.unwrap();
    connection.abort();
    release.send(()).unwrap();
    eventually("the claim settles", || {
        team.registry()
            .inbox()
            .open_tasks_of(&team.worker_pane)
            .is_empty()
    })
    .await;
    let again = reply(team.report_task(&task, "done").await);
    assert_eq!(again.disposition, WorkDoneDisposition::AlreadyAccepted);
    assert_eq!(team.effects().claims, 0);
}

impl Team {
    /// How many notices with `subject` the orchestrator holds that name
    /// `task`.
    fn notices_naming(&self, subject: &str, task: &str) -> usize {
        self.registry()
            .inbox()
            .bodies_for_test(&self.orch_pane)
            .into_iter()
            .filter(|(s, body)| s == subject && body.contains(task))
            .count()
    }

    /// Start a report of `task` held at `pause`.
    async fn paused(
        self: &Arc<Self>,
        task: &str,
        pause: &str,
    ) -> (
        tokio::task::JoinHandle<Completion>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, release) = self.registry().pause_at_for_test(pause);
        let report = {
            let team = Arc::clone(self);
            let task = task.to_string();
            tokio::spawn(async move { team.report_task(&task, "done").await })
        };
        reached.await.unwrap();
        (report, release)
    }

    async fn worker_exits(&self) {
        kill_child(self.registry(), &self.worker);
        eventually("the worker's exit is walked", || {
            self.registry()
                .commission_arm_ids_for_test(&self.worker_pane)
                .is_empty()
        })
        .await;
        // The exit's notices are posted after the walk.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

const EXITED: &str = "worker exited";
const UNDELIVERED: &str = "task not delivered";

/// Scenario: Through the worker's real exit: (a) an acknowledged task whose report is paused when the worker exits, then fails its save: its "worker exited" notice is withheld at the exit and posted once by the failure, the task is closed, and a repeat reads `task_closed`. (c) a newer acknowledged task beside the claimed one: its notice is posted at the exit, the claimed task's by the failure. (d) as (a) with the save succeeding: the claimed task's notice is never posted. (e) an unacknowledged claimed task: its undeliverable notice is withheld and posted once by the failure. (f) a failure before the exit: the task is open at the exit, which posts its notice once.
#[spec("orchestration/work-done/045")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_045_a_save_failure_after_the_workers_exit() {
    // (a), (d): acknowledged, with the save failing or not.
    for fail in [true, false] {
        let team = Arc::new(team(&format!("wd045a-{fail}")));
        let t1 = team.delegate("t1");
        team.acknowledge();
        if fail {
            team.registry()
                .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
        }
        let (report, release) = team.paused(&t1, crate::task_completion::PAUSE_SAVED).await;
        team.worker_exits().await;
        assert_eq!(
            team.notices_naming(EXITED, &t1),
            0,
            "withheld during the save"
        );
        release.send(()).unwrap();
        let outcome = reply(report.await.unwrap());
        tokio::time::sleep(Duration::from_millis(150)).await;
        if fail {
            assert_eq!(outcome.code.as_deref(), Some(code::REPORT_SAVE_FAILED));
            assert_eq!(
                team.notices_naming(EXITED, &t1),
                1,
                "posted once, by the failure"
            );
            refused_with(team.report_task(&t1, "done").await, code::TASK_CLOSED);
        } else {
            assert_eq!(outcome.disposition, WorkDoneDisposition::Accepted);
            assert_eq!(team.notices_naming(EXITED, &t1), 0, "never posted");
        }
    }

    // (c): a newer acknowledged task beside the claimed one.
    let team = Arc::new(team("wd045c"));
    let t1 = team.delegate("t1");
    team.acknowledge();
    team.registry()
        .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
    let (report, release) = team.paused(&t1, crate::task_completion::PAUSE_SAVED).await;
    let t2 = team.delegate("t2");
    team.acknowledge();
    team.worker_exits().await;
    assert_eq!(team.notices_naming(EXITED, &t2), 1, "t2's at the exit");
    assert_eq!(team.notices_naming(EXITED, &t1), 0);
    release.send(()).unwrap();
    refused_with(report.await.unwrap(), code::REPORT_SAVE_FAILED);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(team.notices_naming(EXITED, &t1), 1, "t1's by the failure");
    assert_eq!(team.notices_naming(EXITED, &t2), 1);

    // (e): an unacknowledged claimed task.
    let team = Arc::new(super::report_tests::team("wd045e"));
    let t1 = team.delegate("t1");
    team.registry()
        .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
    let (report, release) = team.paused(&t1, crate::task_completion::PAUSE_SAVED).await;
    team.worker_exits().await;
    assert_eq!(team.notices_naming(UNDELIVERED, &t1), 0, "none at the exit");
    release.send(()).unwrap();
    refused_with(report.await.unwrap(), code::REPORT_SAVE_FAILED);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        team.notices_naming(UNDELIVERED, &t1),
        1,
        "one, by the failure"
    );

    // (f): the failure first, then the exit.
    let team = super::report_tests::team("wd045f");
    let t1 = team.delegate("t1");
    team.acknowledge();
    team.registry()
        .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
    refused_with(
        team.report_task(&t1, "done").await,
        code::REPORT_SAVE_FAILED,
    );
    assert_eq!(
        team.registry().inbox().open_tasks_of(&team.worker_pane),
        vec![t1.clone()]
    );
    team.worker_exits().await;
    assert_eq!(
        team.notices_naming(EXITED, &t1),
        1,
        "posted once, by the exit"
    );
}

/// Scenario: Plain exits with no claim, through the real EOF, counting the orchestrator's notices by subject and task id: (i) an unacknowledged task gets one "not delivered" notice; (j) an acknowledged task one "worker exited" notice and no undeliverable notice; (l) an explicit close of the worker gets no "worker exited" notice, and an unacknowledged task's close gets the one undeliverable notice today's code gives.
#[spec("orchestration/work-done/045")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_045_plain_exits_owe_what_their_tasks_owe() {
    // (i)
    let team = team("wd045i");
    let t = team.delegate("unacknowledged");
    team.worker_exits().await;
    assert_eq!(
        (
            team.notices_naming(UNDELIVERED, &t),
            team.notices_naming(EXITED, &t)
        ),
        (1, 0)
    );
    // (j)
    let team = super::report_tests::team("wd045j");
    let t = team.delegate("acknowledged");
    team.acknowledge();
    team.worker_exits().await;
    assert_eq!(
        (
            team.notices_naming(UNDELIVERED, &t),
            team.notices_naming(EXITED, &t)
        ),
        (0, 1)
    );
    // (l) an explicit close.
    for acknowledged in [true, false] {
        let team = super::report_tests::team(&format!("wd045l-{acknowledged}"));
        let t = team.delegate("task");
        if acknowledged {
            team.acknowledge();
        }
        drop(team.registry().begin_pane_close(&team.worker_pane));
        let _ = team.registry().close_agent(&team.worker);
        drop(team.registry().finish_pane_close(&team.worker_pane, true));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            team.notices_naming(EXITED, &t),
            0,
            "a close owes no exit notice"
        );
        assert_eq!(
            team.notices_naming(UNDELIVERED, &t),
            usize::from(!acknowledged),
            "an unacknowledged task's close gives today's one undeliverable notice"
        );
    }
}

/// Scenario: A save task that panics, at each phase, settled by its supervisor; each arm asserts what the reporting connection reads. (a) before staging: `report_save_failed`, the claim gone and the task open again. (b) after staging and (c) after the unit mark: rolled forward and accepted. (e) after the commit's section returned and before the handover: accepted, from the committed state. (e') in the helpers after the handover: accepted. (i) a note panicking before the handover reads `uncorrelated` to the inbox.
#[spec("orchestration/work-done/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_053_a_panicking_save_task_is_settled_by_its_supervisor() {
    use crate::task_completion::{
        FAULT_AFTER_HANDOVER, FAULT_BEFORE_STAGE, FAULT_COMMIT_RETURNED, FAULT_MARKED, FAULT_STAGED,
    };
    for (seam, accepted_expected) in [
        (FAULT_BEFORE_STAGE, false),
        (FAULT_STAGED, true),
        (FAULT_MARKED, true),
        (FAULT_COMMIT_RETURNED, true),
        (FAULT_AFTER_HANDOVER, true),
    ] {
        let team = team(&format!("wd053-{seam}"));
        let task = team.delegate("a task");
        team.registry()
            .inject_fault_for_test(seam, TestFault::Panic("injected-053"));
        let outcome = reply(team.report_task(&task, "done").await);
        if accepted_expected {
            assert_eq!(outcome.disposition, WorkDoneDisposition::Accepted, "{seam}");
            assert!(
                team.registry()
                    .inbox()
                    .open_tasks_of(&team.worker_pane)
                    .is_empty()
            );
        } else {
            assert_eq!(
                outcome.code.as_deref(),
                Some(code::REPORT_SAVE_FAILED),
                "{seam}"
            );
            assert_eq!(
                team.registry().inbox().open_tasks_of(&team.worker_pane),
                vec![task.clone()]
            );
            accepted(team.report_task(&task, "done").await);
        }
        assert_eq!(team.effects().claims, 0, "{seam}");
    }
    // (i) a note.
    let team = team("wd053i");
    team.registry()
        .inject_fault_for_test(FAULT_COMMIT_RETURNED, TestFault::Panic("injected-053i"));
    let note = reply(team.report(WorkDoneMode::Uncorrelated, "a note").await);
    assert_eq!(note.disposition, WorkDoneDisposition::Uncorrelated);
    assert_eq!(note.destination, Some(Destination::Inbox));
}

/// Scenario: Worker A, started in its own directory, acknowledges task t and reports it; the report attests and is held before it reads the role map. A exits with its exit cleanup held, so t stays open, and B starts on A's pane in another directory and another orchestration instance; the role map now describes B, and B's own `work-done-coder.md`, a report the deck filed and would replace, is on disk. Released, A's report is accepted for t; B's file is unchanged, A's copy lands in A's directory, and the retained report names A's instance and A as its producer. Run again with A's record removed while the save is paused after admission: the same.
#[spec("orchestration/work-done/058")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_058_a_delayed_task_report_never_uses_its_successors_pane() {
    for remove_record in [false, true] {
        let mut team = team(&format!("wd058-{}", u8::from(remove_record)));
        let dir_a = crate::test_temp::tempdir().unwrap();
        let dir_b = crate::test_temp::tempdir().unwrap();
        let path = |dir: &tempfile::TempDir| {
            dir.path()
                .join(".dot-agent-deck")
                .join("work-done-coder.md")
        };
        // A, in its own directory, replaces the fixture's worker.
        let _ = team.registry().close_agent(&team.worker);
        let spawn_in = {
            let registry = Arc::clone(team.registry());
            let pane = team.worker_pane.clone();
            move |dir: &tempfile::TempDir| {
                registry.spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    cwd: Some(dir.path().to_str().unwrap()),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.clone())],
                    ..SpawnOptions::default()
                })
            }
        };
        let a = spawn_in(&dir_a).expect("A");
        team.worker = a.clone();
        team.state.pane_cwd_map.insert(
            team.worker_pane.clone(),
            dir_a.path().to_string_lossy().into_owned(),
        );
        let task = team.delegate("A's task");
        team.acknowledge();
        let state = Arc::new(tokio::sync::RwLock::new(team.state.clone()));
        let provenance = team.attested(&a);
        let held = team.registry().hold_exit_cleanup_for_test(&a);
        let (reached, release) = team
            .registry()
            .pause_at_for_test(crate::task_completion::PAUSE_ATTESTED);
        let report = {
            let registry = Arc::clone(team.registry());
            let state = Arc::clone(&state);
            let pane = team.worker_pane.clone();
            let task = task.clone();
            tokio::spawn(async move {
                crate::task_completion::complete(
                    &state,
                    &registry,
                    CompletionRequest {
                        pane_id: pane,
                        mode: WorkDoneMode::Task { task_id: task },
                        report: "A-058-REPORT".to_string(),
                    },
                    &provenance,
                )
                .await
            })
        };
        reached.await.unwrap();
        // A exits; its cleanup, the task-ledger walk included, is held.
        kill_child(team.registry(), &a);
        let mut b = None;
        eventually("B starts on A's pane", || {
            b = spawn_in(&dir_b).ok();
            b.is_some()
        })
        .await;
        {
            let mut st = state.write().await;
            st.pane_cwd_map.insert(
                team.worker_pane.clone(),
                dir_b.path().to_string_lossy().into_owned(),
            );
            st.pane_orchestration_map.insert(
                team.worker_pane.clone(),
                OrchestrationIdentity {
                    id: "wd058-successor-instance".to_string(),
                    name: "other".to_string(),
                },
            );
        }
        // B's earlier report, as the deck files one: the deck replaces it.
        let b_report = crate::state::frame_untrusted_report_for_file("B-058-SENTINEL");
        std::fs::create_dir_all(path(&dir_b).parent().unwrap()).unwrap();
        std::fs::write(path(&dir_b), &b_report).unwrap();
        let saved = remove_record.then(|| {
            team.registry()
                .pause_at_for_test(crate::task_completion::PAUSE_SAVED)
        });
        release.send(()).unwrap();
        if let Some((saved, go)) = saved {
            saved.await.unwrap();
            let _ = team.registry().close_agent(&a);
            assert!(
                team.registry().agent_record_any(&a).is_none(),
                "A's record is gone"
            );
            go.send(()).unwrap();
        }
        let answer = accepted(report.await.unwrap());
        assert_eq!(answer.task_id.as_deref(), Some(task.as_str()));
        assert_eq!(
            std::fs::read_to_string(path(&dir_b)).unwrap(),
            b_report,
            "B's file is untouched"
        );
        assert_eq!(
            std::fs::read_dir(path(&dir_b).parent().unwrap())
                .unwrap()
                .count(),
            1,
            "nothing else in B's directory"
        );
        assert!(
            std::fs::read_to_string(path(&dir_a))
                .unwrap()
                .contains("A-058-REPORT"),
            "A's copy is in A's directory"
        );
        let report_id = answer.report_id.unwrap();
        let file = std::fs::File::open(
            team.deck
                .store
                .dir()
                .join(crate::retained_store::own_name(&report_id)),
        )
        .unwrap();
        let envelope = crate::retained_store::read_attribution(file).expect("an envelope");
        assert_eq!(envelope.instance.as_deref(), Some(team.instance.as_str()));
        assert_eq!(envelope.producer_agent_id, a);
        drop(held);
        let _ = team.registry().close_agent(b.as_deref().unwrap());
    }
}

/// Scenario: A task report's save fails while the delegation tracker's lock is poisoned: the failure writes nothing before it finds the tracker unavailable, so the connection reads no answer and the claim, the task, its delivery watch, the reply room, the orchestrator's inbox, the rows and the store's reservations are as they were.
#[spec("orchestration/work-done/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_053_l_an_unavailable_tracker_before_a_failure_writes_nothing() {
    let team = Arc::new(team("wd053l"));
    let task = team.delegate("a task");
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_SAVED);
    let report = {
        let team = Arc::clone(&team);
        let task = task.clone();
        tokio::spawn(async move { team.report_task(&task, "done").await })
    };
    reached.await.unwrap();
    let state = |team: &Team| {
        let hub = team.registry().inbox();
        (
            hub.reserved_replies(),
            hub.read_reports(|r| {
                r.claims
                    .iter()
                    .map(|c| format!("{:?} {} {}", c.phase, c.room, c.reserved_bytes))
                    .collect::<Vec<_>>()
            })
            .unwrap(),
            hub.open_tasks_of(&team.worker_pane),
            hub.watched_tasks_for_test(),
            team.orch_subjects(),
            rows(&team.deck).len(),
            team.deck
                .store
                .counts()
                .map(|c| (c.reserved_bytes, c.reserved_slots)),
        )
    };
    let before = state(&team);
    let registry = Arc::clone(team.registry());
    let _ = std::thread::spawn(move || {
        let _held = registry.delegations.lock();
        panic!("poison the delegation tracker (test)");
    })
    .join();
    team.registry()
        .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
    release.send(()).unwrap();
    assert_eq!(report.await.unwrap(), Completion::Unconfirmed);
    let after = state(&team);
    assert_eq!(after, before, "the failure wrote nothing");
    assert_eq!(before.1.len(), 1, "the claim stays where it was");
    // Leaked on purpose: the registry's own teardown panics on a poisoned
    // lock, the documented boundary.
    std::mem::forget(Arc::try_unwrap(team).ok());
}

/// Scenario: (h) the after-section helpers run only after the answer is handed over. While a helper is held inside `after_section`, the reporting connection already reads `accepted`. A helper that panics there leaves the answer `accepted`, the supervisor logs the save task's panic, and the deck goes on taking reports.
#[spec("orchestration/work-done/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_053_h_the_helpers_run_after_the_handover() {
    let team = Arc::new(team("wd053h"));
    let task = team.delegate("a task");
    let (reached, in_helper) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    *team.registry().inbox().after_section_hook.lock().unwrap() = Some(Box::new(move || {
        let _ = reached.send(());
        // Hands this worker's queued tasks to another thread while it waits,
        // as a slow helper on the blocking pool would leave them runnable.
        tokio::task::block_in_place(|| {
            let _ = held.recv();
        });
    }));
    let report = {
        let team = Arc::clone(&team);
        let task = task.clone();
        tokio::spawn(async move { team.report_task(&task, "done").await })
    };
    tokio::task::spawn_blocking(move || in_helper.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the helper is reached");
    let answer = tokio::time::timeout(Duration::from_secs(5), report)
        .await
        .expect("answered while a helper is still running")
        .unwrap();
    accepted(answer);
    release.send(()).unwrap();

    // A helper that panics.
    let task = team.delegate("another task");
    *team.registry().inbox().after_section_hook.lock().unwrap() =
        Some(Box::new(|| std::panic::panic_any("injected-053h")));
    accepted(team.report_task(&task, "done").await);
    let task = team.delegate("a third task");
    accepted(team.report_task(&task, "done").await);
}

/// Scenario: (m) a worker exits owing task t, whose own commission arm is already gone, while a sibling arm, armed for another delegate, is still owed on the same pane. The exit's retirement is exact: it removes nothing for t's missing arm, and the sibling arm is still owed afterwards.
#[spec("orchestration/work-done/045")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_045_m_an_exit_never_retires_a_sibling_arm() {
    let team = team("wd045m");
    let _task = team.delegate("t");
    team.acknowledge();
    let own = team
        .registry()
        .commission_arm_ids_for_test(&team.worker_pane);
    assert_eq!(own.len(), 1, "t's own arm");
    assert!(
        team.registry()
            .delegations
            .lock()
            .remove_arm_exact(&team.worker_pane, own[0]),
        "t's arm is gone before the exit"
    );
    let sibling = match team.registry().arm_delegation_commission(
        &team.worker_pane,
        &team.orch_pane,
        Some(&team.orch),
        false,
    ) {
        CommissionArm::Armed { in_flight, .. } => in_flight.arm_id(),
        _ => panic!("a sibling arm"),
    };
    assert_eq!(
        team.registry()
            .commission_arm_ids_for_test(&team.worker_pane),
        vec![sibling]
    );
    kill_child(team.registry(), &team.worker);
    eventually("the exit walks t", || {
        team.registry()
            .inbox()
            .open_tasks_of(&team.worker_pane)
            .is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        team.registry()
            .commission_arm_ids_for_test(&team.worker_pane),
        vec![sibling],
        "the sibling arm is still owed"
    );
}

/// Scenario: A client subscribes to the deck's broadcast. A worker's task report is accepted, and every notice the hub broadcasts for the report's message — when it is posted, handed over and acknowledged — carries the report's provenance, naming the task, the report id and the worker, including a hand-over that came back; a deck notice posted beside it carries none.
#[spec("orchestration/work-done/059")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_059_a_reports_broadcast_notices_carry_its_provenance() {
    use crate::report_recovery::ReportProvenance;
    let team = team("wd059");
    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    team.registry().inbox().set_broadcast(tx);
    let task = team.delegate("a task");
    let report = accepted(team.report_task(&task, "done").await);
    let report_id = report.report_id.clone().unwrap();
    let message = team
        .registry()
        .inbox()
        .list(&team.orch_pane)
        .into_iter()
        .find(|m| m.subject == format!("coder completed task {task}"))
        .expect("the report's message")
        .id
        .to_string();
    team.registry()
        .inbox()
        .post_direct(
            Recipient::bound(&team.orch_pane, &team.orch),
            Content::from_deck("notice", "a deck notice", "not a report"),
        )
        .unwrap();
    // One hand-over that comes back, then one that is acknowledged.
    let hub = team.registry().inbox();
    let hook = hub.test_hook_fetch(&team.orch_pane, &team.orch, None);
    hub.test_hook_not_handed(&hook);
    hub.test_hook_close(hook);
    hub.test_take_acked(&team.orch_pane, &team.orch);
    let mut states = Vec::new();
    let mut returned = 0;
    let mut other = 0;
    while let Ok(msg) = rx.try_recv() {
        let crate::event::BroadcastMsg::Inbox(notice) = msg else {
            continue;
        };
        if notice.message_id == message {
            match &notice.provenance {
                Some(ReportProvenance::Task {
                    task_id,
                    report_id: id,
                    producer,
                }) => {
                    assert_eq!(task_id, &task);
                    assert_eq!(id, &report_id);
                    assert_eq!(producer.agent_id, team.worker);
                }
                other => panic!("the report's notice carries {other:?}"),
            }
            states.push(notice.state);
            returned += usize::from(notice.returned);
        } else if notice.pane_id == team.orch_pane && notice.subject == "a deck notice" {
            assert_eq!(notice.provenance, None);
            other += 1;
        }
    }
    assert!(states.contains(&StateName::Queued), "{states:?}");
    assert!(states.contains(&StateName::Fetched), "{states:?}");
    assert!(states.contains(&StateName::Acknowledged), "{states:?}");
    assert!(other > 0, "the deck notice was broadcast too");
    // An offer that leaves its holder for another generation is broadcast
    // with `returned`; this schedule hands it to one generation only.
    assert_eq!(returned, 0);
}

/// Scenario: (d) a delayed note across pane reuse into another orchestration: worker A, spawned into the team's instance, sends a note that attests and is held before it reads the role map. A exits, B starts on A's pane in another instance, and that instance's orchestrator is running; the role map now puts the pane in the other instance. Released, the note is refused as from a pane that now belongs to another orchestration: neither orchestrator's inbox gets it, and nothing is saved or listed.
#[spec("orchestration/work-done/048")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_048_d_a_delayed_note_never_crosses_into_another_instance() {
    let mut team = team("wd048d");
    let registry = Arc::clone(team.registry());
    let seated = move |pane: &str, instance: &str, orchestrator: bool| {
        registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                tab_membership: Some(TabMembership::Orchestration {
                    name: "team".to_string(),
                    role_index: usize::from(!orchestrator),
                    role_name: if orchestrator {
                        "orchestrator"
                    } else {
                        "coder"
                    }
                    .to_string(),
                    is_start_role: orchestrator,
                    orchestration_cwd: None,
                    display_title: None,
                    orchestration_id: Some(instance.to_string()),
                }),
                origin: Some(crate::hook_provenance::InstanceOrigin::Direct),
                ..SpawnOptions::default()
            })
            .expect("spawn a seated cat")
    };
    let _ = team.registry().close_agent(&team.worker);
    let a = seated(&team.worker_pane, &team.instance, false);
    team.worker = a.clone();
    let provenance = team.attested(&a);
    assert!(
        matches!(&provenance, Provenance::Attested { facts, .. }
            if facts.seat.as_ref().is_some_and(|seat| seat.instance == team.instance)),
        "A is seated in the team's instance"
    );
    let state = Arc::new(tokio::sync::RwLock::new(team.state.clone()));
    let (reached, release) = team
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_ATTESTED);
    let note = {
        let registry = Arc::clone(team.registry());
        let state = Arc::clone(&state);
        let pane = team.worker_pane.clone();
        tokio::spawn(async move {
            crate::task_completion::complete(
                &state,
                &registry,
                CompletionRequest {
                    pane_id: pane,
                    mode: WorkDoneMode::Uncorrelated,
                    report: "A-048D-NOTE".to_string(),
                },
                &provenance,
            )
            .await
        })
    };
    reached.await.unwrap();
    kill_child(team.registry(), &a);
    let other = "wd048d-other-instance";
    let orch2_pane = "wd048d-orch2";
    let orch2 = seated(orch2_pane, other, true);
    let mut b = None;
    eventually("B starts on A's pane", || {
        b = team
            .registry()
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), team.worker_pane.clone())],
                ..SpawnOptions::default()
            })
            .ok();
        b.is_some()
    })
    .await;
    {
        let mut st = state.write().await;
        let identity = OrchestrationIdentity {
            id: other.to_string(),
            name: "other".to_string(),
        };
        st.pane_orchestration_map
            .insert(team.worker_pane.clone(), identity.clone());
        st.pane_orchestration_map
            .insert(orch2_pane.to_string(), identity);
        st.pane_role_map
            .insert(orch2_pane.to_string(), "orchestrator".to_string());
        st.orchestrator_pane_ids.insert(orch2_pane.to_string());
    }
    release.send(()).unwrap();
    refused_with(note.await.unwrap(), code::NOT_A_WORKER);
    let holds_note = |pane: &str| {
        team.registry()
            .inbox()
            .rendered(pane)
            .into_iter()
            .any(|delivery| delivery.text.contains("A-048D-NOTE"))
    };
    assert!(
        !holds_note(orch2_pane),
        "not the other instance's orchestrator"
    );
    assert!(
        !holds_note(&team.orch_pane),
        "not the team's orchestrator either"
    );
    assert!(team.registry().inbox().list(orch2_pane).is_empty());
    assert_eq!(store_files(&team.deck.store).len(), 0);
    assert!(rows(&team.deck).is_empty());
    let _ = (orch2, b);
}

/// The store's own files, by name.
pub(super) fn store_files(store: &crate::retained_store::RetainedStore) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(store.dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|file| file.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// What a restart of `deck` would adopt: its retained tree copied, lock files
/// unheld, into a fresh state directory that a new store opens. Answers the
/// new run's adopted names; the copy is removed on return.
pub(super) fn adopted_after_restart(deck: &Deck) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt as _;
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir(to).unwrap();
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700)).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
    deck.store.flush_inventory().unwrap();
    let root = crate::test_temp::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    copy(
        &deck.root.path().join("state").join("retained-reports"),
        &state.join("retained-reports"),
    );
    let next = crate::retained_store::RetainedStore::open(
        &state,
        crate::retained_store::StoreConfig::default(),
        crate::retained_store::Now::current(),
    );
    store_files(&next)
}

/// Scenario: (a) in full: a save task panics before its write, or after its report's file was written and before staging. The connection reads `report_save_failed`, and the attempt leaves nothing: the store's files, bytes, reservations and slots are what they were before the report. A retry is accepted, and a restart adopts exactly the retry's report. When the attempt's file cannot be removed, it stays charged as an orphaned attempt, and a restart adopts it as orphaned, never as a report. A write that unwinds returns its room once.
#[spec("orchestration/work-done/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_053_a_a_failed_attempt_leaves_no_report_behind() {
    use crate::retained_store::{AdoptedState, OwnState, parse_adopted_name, parse_own_name};
    use crate::task_completion::{FAULT_BEFORE_STAGE, FAULT_BEFORE_WRITE};
    for (seam, unlink_fails) in [
        (FAULT_BEFORE_WRITE, false),
        (FAULT_BEFORE_STAGE, false),
        (FAULT_BEFORE_STAGE, true),
    ] {
        let team = team(&format!("wd053a-{seam}-{}", u8::from(unlink_fails)));
        let task = team.delegate("a task");
        let store = Arc::clone(&team.deck.store);
        let counts_before = store.counts().unwrap();
        let files_before = store_files(&store);
        team.registry()
            .inject_fault_for_test(seam, TestFault::Panic("injected-053a"));
        store.fail_unlinks_for_test(unlink_fails);
        let refusal = refused_with(
            team.report_task(&task, "done").await,
            code::REPORT_SAVE_FAILED,
        );
        assert!(
            refusal
                .message
                .as_deref()
                .is_some_and(|m| m.contains("saved nothing new")),
            "{refusal:?}"
        );
        let after = store.counts().unwrap();
        let attempt: Vec<String> = store_files(&store)
            .into_iter()
            .filter(|name| !files_before.contains(name))
            .collect();
        assert_eq!(after.reserved_bytes, counts_before.reserved_bytes);
        assert_eq!(after.reserved_slots, counts_before.reserved_slots);
        if unlink_fails {
            assert_eq!(attempt.len(), 1, "the attempt's file could not go");
            let id = parse_own_name(&attempt[0]).unwrap();
            assert_eq!(store.own_state(id), Some(OwnState::OrphanUnlink));
            assert_eq!(after.own_files, counts_before.own_files + 1);
            store.fail_unlinks_for_test(false);
        } else {
            assert_eq!(attempt, Vec::<String>::new(), "no report artifact");
            assert_eq!(after, counts_before, "the room, bytes and slot came back");
        }
        assert_eq!(
            team.registry().inbox().open_tasks_of(&team.worker_pane),
            vec![task.clone()]
        );
        assert_eq!(team.effects().claims, 0);
        let retry = accepted(team.report_task(&task, "done").await);
        let retry_id = retry.report_id.clone().unwrap();
        let mut adopted: Vec<(String, char)> = adopted_after_restart(&team.deck)
            .iter()
            .map(|name| {
                let (id, _, state) = parse_adopted_name(name).unwrap();
                (id.to_string(), state.letter())
            })
            .collect();
        adopted.sort();
        let mut expected = vec![(retry_id, AdoptedState::Pinned.letter())];
        if unlink_fails {
            let id = parse_own_name(&attempt[0]).unwrap().to_string();
            expected.push((id, AdoptedState::Orphaned.letter()));
            expected.sort();
        }
        assert_eq!(adopted, expected, "exactly the retry's report");
    }
    // A write that unwinds (here its debug check of the report id) returns
    // its room once.
    let deck = deck();
    let before = deck.store.counts().unwrap();
    let room = deck.store.reserve(64).unwrap();
    let store = Arc::clone(&deck.store);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.write(
            room,
            "not a report id",
            b"x",
            crate::retained_store::Now::current(),
        );
    }));
    assert!(unwound.is_err(), "the write's debug check fires");
    assert_eq!(deck.store.counts().unwrap(), before);
}

/// Scenario: (d) the commit panics inside its section, after the report's post and before its tombstone. The supervisor's task ends without panicking, the reporting connection reads no answer (unconfirmed), the report state stays torn, the inserted message is the only one, the report's file is on disk, and a repeat is refused `deck_unavailable`. (g) a failure asked for a staged claim refuses and changes nothing.
#[spec("orchestration/work-done/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_053_d_g_a_panic_inside_the_commit_and_a_staged_failure() {
    let team = team("wd053d");
    let task = team.delegate("a task");
    crate::inbox_hub::claim_seams::on(
        crate::inbox_hub::claim_seams::APPLY_AFTER_POST,
        &team.worker_pane,
        || std::panic::panic_any("injected-apply"),
    );
    assert_eq!(
        team.report_task(&task, "done").await,
        Completion::Unconfirmed
    );
    assert!(team.registry().inbox().reports_torn());
    assert_eq!(
        team.orch_subjects(),
        vec![format!("coder completed task {task}")],
        "the inserted message is the only one"
    );
    assert_eq!(std::fs::read_dir(team.deck.store.dir()).unwrap().count(), 1);
    refused_with(
        team.report_task(&task, "done").await,
        code::DECK_UNAVAILABLE,
    );
    // Leaked on purpose: its locks are poisoned, and teardown is the
    // documented boundary.
    std::mem::forget(team);

    // (g)
    let team = super::report_tests::team("wd053g");
    let task = team.delegate("a task");
    let spec = crate::inbox_hub::ClaimSpec {
        worker_pane: team.worker_pane.clone(),
        worker_agent: team.worker.clone(),
        digest: [7; 32],
        report_id: crate::retained_store::mint_report_id(),
        file_bytes: 4096,
    };
    let crate::inbox_hub::Admission::Claimed(claimed) =
        team.registry().inbox().admit_task_claim(&spec, &task, None)
    else {
        panic!("a claim");
    };
    team.registry()
        .inbox()
        .stage_claim(
            claimed.claim_id,
            Content::from_deck("notice", "staged", "staged"),
        )
        .unwrap();
    let (outcome, _) = team.registry().fail_claim(claimed.claim_id);
    assert_eq!(outcome, crate::inbox_hub::FailOutcome::Staged);
    assert_eq!(
        team.registry()
            .inbox()
            .claim_for_test(claimed.claim_id, |c| c.phase),
        Some(crate::report_recovery::ClaimPhase::Staged)
    );
    team.deck.store.release(claimed.reservation);
}

/// One hand-over and one history row per pane, for the rest of this test
/// process (a nextest process runs one test).
fn one_hand_over_and_one_history_row() {
    // SAFETY: a nextest process runs one test.
    unsafe {
        std::env::set_var(crate::inbox::ENV_MAX_DELIVER, "1");
        std::env::set_var(crate::inbox::ENV_HISTORY_PER_PANE, "1");
    }
}

/// Hand the orchestrator its report once and let the hand-over lapse with
/// nothing confirmed: one hand-over is all it gets, so it is undeliverable.
fn lapse_the_only_hand_over(team: &Team) -> String {
    let hub = team.registry().inbox();
    let hook = hub.test_hook_fetch(&team.orch_pane, &team.orch, None);
    let ids = hook.ids();
    assert_eq!(ids.len(), 1, "the report was handed over");
    hub.test_hook_not_handed(&hook);
    hub.test_hook_close(hook);
    ids.into_iter().next().unwrap()
}

/// Scenario: A report runs out of hand-overs (one), more messages push it out of the orchestrator's history, and then the orchestrator exits: the report's recovery row exists, marked `delivery_failed`, and its saved file still holds the report.
#[spec("orchestration/work-done/049")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_049_a_report_out_of_hand_overs_survives_history_and_exit() {
    one_hand_over_and_one_history_row();
    let team = team("wd049");
    let task = team.delegate("a task");
    accepted(team.report_task(&task, "SENTINEL-wd049").await);
    let _id = lapse_the_only_hand_over(&team);
    for n in 0..3 {
        team.registry()
            .inbox()
            .post_direct(
                Recipient::bound(&team.orch_pane, &team.orch),
                Content::from_deck("notice", format!("filler {n}"), "filler"),
            )
            .unwrap();
        let _ = team
            .registry()
            .inbox()
            .test_take_acked(&team.orch_pane, &team.orch);
    }
    kill_child(team.registry(), &team.orch);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let kept: Vec<_> = rows(&team.deck)
        .into_iter()
        .filter(|row| row.task_id.as_deref() == Some(task.as_str()))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].cause, OutcomeCause::DeliveryFailed);
    let file = team
        .deck
        .store
        .dir()
        .join(format!("r-{}.md", kept[0].report_id.as_deref().unwrap()));
    assert!(
        std::fs::read_to_string(file)
            .unwrap()
            .contains("SENTINEL-wd049")
    );
}

/// Scenario: A report runs out of hand-overs, and the orchestrator's pane is then closed, which forgets its settled history: the row exists. A late acknowledgement after the hand-overs ran out marks the row `acknowledged_late`.
#[spec("orchestration/work-done/050")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_050_a_report_out_of_hand_overs_survives_a_close_and_a_late_ack() {
    one_hand_over_and_one_history_row();
    let team = team("wd050");
    let task = team.delegate("a task");
    accepted(team.report_task(&task, "done").await);
    // The one hand-over is reported not handed, so the report runs out of
    // hand-overs; then the agent's commit says it got it after all.
    let hub = team.registry().inbox();
    let hook = hub.test_hook_fetch(&team.orch_pane, &team.orch, None);
    let id = hook.ids().into_iter().next().expect("handed over");
    hub.test_hook_not_handed(&hook);
    assert!(
        rows(&team.deck)
            .iter()
            .any(|row| row.task_id.as_deref() == Some(task.as_str())),
        "out of hand-overs, it is a row"
    );
    hub.test_hook_committed(&hook, &[id.as_str()]);
    hub.test_hook_close(hook);
    let late = rows(&team.deck)
        .into_iter()
        .find(|row| row.task_id.as_deref() == Some(task.as_str()))
        .expect("a row");
    assert_eq!(
        late.delivery,
        crate::report_recovery::OutcomeDelivery::AcknowledgedLate
    );
    drop(team.registry().begin_pane_close(&team.orch_pane));
    let _ = team.registry().close_agent(&team.orch);
    drop(team.registry().finish_pane_close(&team.orch_pane, true));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        rows(&team.deck)
            .iter()
            .any(|row| row.task_id.as_deref() == Some(task.as_str())),
        "the row outlives the close"
    );
}
