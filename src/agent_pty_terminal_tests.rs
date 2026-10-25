//! PRD #1666 part 3, round 5 (plan §2 step 2, D7, D12 rules 7–8, §6.2): a
//! dispatched unit's terminal report (`work-done --done`) through the real
//! transaction, against its unit's exit, close, respawn and registration,
//! with `cat` stand-ins for every agent.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use spec::spec;

use super::dispatch_lifecycle_tests::{
    Deck, deck, dispatch_orchestration, dispatch_single, eventually, inbox_of, kill_child, rows,
    spawn_cat, unit_entry,
};
use super::report_tests::{refused_with, reply};
use super::*;
use crate::event::{WorkDoneDisposition, WorkDoneMode};
use crate::hook_provenance::{AttestFacts, InstanceOrigin, Provenance};
use crate::report_recovery::{Destination, EndedHow, UnitEntry, UnitMark};
use crate::spawn::SpawnHandle;
use crate::state::AppState;
use crate::task_completion::{Completion, CompletionRequest};
use crate::work_done_text::code;

/// A dispatched unit and the caller that dispatched it.
struct Unit {
    deck: Deck,
    caller_pane: String,
    pane: String,
    agent: String,
    unit_id: String,
    orchestration_id: Option<String>,
}

async fn unit(tag: &str, orchestration: bool) -> Unit {
    let deck = deck();
    let caller_pane = format!("{tag}-caller");
    let caller = spawn_cat(&deck.registry, &caller_pane);
    let handle = if orchestration {
        dispatch_orchestration(&deck, tag, &caller_pane, &caller).await
    } else {
        dispatch_single(&deck, tag, &caller_pane, &caller).await
    };
    let handle: SpawnHandle = handle;
    Unit {
        deck,
        caller_pane,
        pane: handle.delivery_pane_id.clone(),
        agent: handle.delivery_agent_id.clone(),
        unit_id: handle.unit_id.clone().expect("a dispatched unit"),
        orchestration_id: handle.orchestration_id.clone(),
    }
}

impl Unit {
    fn registry(&self) -> &Arc<AgentPtyRegistry> {
        &self.deck.registry
    }

    fn pane(&self) -> &str {
        &self.pane
    }

    fn agent(&self) -> &str {
        &self.agent
    }

    fn unit_id(&self) -> String {
        self.unit_id.clone()
    }

    fn facts(&self, agent: &str) -> AttestFacts {
        self.registry().attest_facts(agent).unwrap_or_default()
    }

    async fn report_from(
        &self,
        pane: &str,
        agent: &str,
        facts: AttestFacts,
        body: &str,
    ) -> Completion {
        crate::task_completion::complete_with_state(
            &AppState::default(),
            self.registry(),
            CompletionRequest {
                pane_id: pane.to_string(),
                mode: WorkDoneMode::Terminal,
                report: body.to_string(),
            },
            &Provenance::Attested {
                agent_id: agent.to_string(),
                facts,
            },
        )
        .await
    }

    async fn report(&self, body: &str) -> Completion {
        self.report_from(self.pane(), self.agent(), self.facts(self.agent()), body)
            .await
    }

    fn completed(&self) -> usize {
        self.count("dispatch completed")
    }

    fn ended(&self) -> usize {
        self.count("dispatch ended")
    }

    fn count(&self, subject: &str) -> usize {
        inbox_of(&self.deck, &self.caller_pane)
            .iter()
            .filter(|(s, _)| s == subject)
            .count()
    }

    /// A deliberate close of the unit's pane, as `StopAgent` runs it.
    fn close(&self) {
        drop(self.registry().begin_pane_close(self.pane()));
        let _ = self.registry().close_agent(self.agent());
        drop(self.registry().finish_pane_close(self.pane(), true));
    }

    fn route_count(&self) -> usize {
        self.registry().outstanding_dispatch_returns()
    }
}

fn accepted(completion: Completion) -> crate::event::WorkDoneTaskReply {
    let reply = reply(completion);
    assert_eq!(
        reply.disposition,
        WorkDoneDisposition::Accepted,
        "{reply:?}"
    );
    reply
}

/// Scenario: A respawned unit's successor starts a terminal report and exits during its paused save; the report reaches the caller once. A predecessor's claimed report likewise survives a failed replacement, for single and orchestration units and for live and exited callers, with no dispatch-ended notice competing with the report.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_c_f_claim_reports_survive_exit_and_failed_replacement() {
    for orchestration in [false, true] {
        for orphaned in [false, true] {
            for failure in [
                None,
                Some(FAULT_SPAWN_FAILS),
                Some(FAULT_RESPAWN_AFTER_SPAWN),
                Some(FAULT_RESPAWN_BEFORE_PUBLISH_SECTION),
                Some(FAULT_SPAWN_AFTER_INSERT),
            ] {
                let mut unit = unit(
                    &format!(
                        "r026cf-{orchestration}-{orphaned}-{}",
                        failure.unwrap_or("exit")
                    ),
                    orchestration,
                )
                .await;
                if failure.is_none() {
                    unit.agent = unit
                        .registry()
                        .respawn_agent_for_pane(unit.pane(), "cat")
                        .await
                        .unwrap();
                }
                if orphaned {
                    let caller = unit
                        .registry()
                        .pane_current_agent_id(&unit.caller_pane)
                        .unwrap();
                    kill_child(unit.registry(), &caller);
                    eventually("the caller exits", || {
                        unit.registry()
                            .pane_current_agent_id(&unit.caller_pane)
                            .is_none()
                    })
                    .await;
                }
                let unit = Arc::new(unit);
                let (report, release) = paused_report(
                    &unit,
                    crate::task_completion::PAUSE_SAVED,
                    "CLAIMED-REPORT-026",
                )
                .await;
                if let Some(seam) = failure {
                    unit.registry().inject_fault_for_test(
                        seam,
                        if seam == FAULT_SPAWN_FAILS {
                            TestFault::Fail
                        } else {
                            TestFault::Panic("claimed-replacement-failure")
                        },
                    );
                    let respawn = {
                        let unit = Arc::clone(&unit);
                        tokio::spawn(async move {
                            unit.registry()
                                .respawn_agent_for_pane(unit.pane(), "cat")
                                .await
                        })
                    }
                    .await;
                    if seam == FAULT_SPAWN_FAILS {
                        assert!(respawn.unwrap().is_err());
                    } else {
                        assert!(respawn.unwrap_err().is_panic());
                    }
                } else {
                    kill_child(unit.registry(), unit.agent());
                }
                eventually("the unit route ends", || unit.route_count() == 0).await;
                assert_eq!(unit.ended(), 0, "the claim withholds dispatch ended");
                release.send(()).unwrap();
                let answer = accepted(report.await.unwrap());
                assert_eq!(
                    answer.destination,
                    Some(if orphaned {
                        Destination::Recovery
                    } else {
                        Destination::Inbox
                    }),
                    "orchestration={orchestration}, orphaned={orphaned}, failure={failure:?}; caller={:?}; rows={:?}",
                    unit.registry().pane_current_agent_id(&unit.caller_pane),
                    rows(&unit.deck)
                );
                assert_eq!(unit.completed(), usize::from(!orphaned));
                assert_eq!(unit.ended(), 0);
                let outcomes = rows(&unit.deck);
                assert_eq!(outcomes.len(), usize::from(orphaned));
                assert!(
                    outcomes
                        .iter()
                        .all(|row| row.phase
                            == crate::report_recovery::OutcomePhase::DispatchFinished)
                );
                assert_eq!(super::report_tests::store_files(&unit.deck.store).len(), 1);
            }
        }
    }
}

/// Scenario: A unit is closed while its replacement is paused before publication, and other ended units push against the tombstone bound. The replacement's terminal report says task_closed both before and after publication, with one dispatch-ended notice and no saved report or resurrected route.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_g_a_closed_replacements_reports_stay_closed_across_publish() {
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var(crate::report_recovery::ENV_TOMBSTONES_MAX, "1") };
    for orchestration in [false, true] {
        let unit = Arc::new(unit(&format!("r026gr-{orchestration}"), orchestration).await);
        let (reached, release) = unit
            .registry()
            .pause_at_sync_for_test(PAUSE_RESPAWN_BEFORE_PUBLISH);
        let respawn = {
            let unit = Arc::clone(&unit);
            tokio::spawn(async move {
                unit.registry()
                    .respawn_agent_for_pane(unit.pane(), "cat")
                    .await
            })
        };
        tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .unwrap();
        let b = unit.registry().pane_current_agent_id(unit.pane()).unwrap();
        let facts = unit.facts(&b);
        drop(unit.registry().begin_pane_close(unit.pane()));
        let others: Vec<_> = (0..32)
            .map(|n| {
                (
                    format!("pressure-p{n}"),
                    format!("gone-{n}"),
                    format!("pressure-u{n}"),
                )
            })
            .collect();
        let refs: Vec<_> = others
            .iter()
            .map(|(p, a, u)| (p.as_str(), a.as_str(), u.as_str()))
            .collect();
        unit.registry()
            .inbox()
            .insert_ended_units_for_test(unit.registry().as_ref(), &refs);
        let answer = refused_with(
            unit.report_from(unit.pane(), &b, facts.clone(), "done")
                .await,
            code::TASK_CLOSED,
        );
        assert_eq!(answer.destination, None);
        release.send(()).unwrap();
        assert_eq!(respawn.await.unwrap().unwrap(), b);
        let answer = refused_with(
            unit.report_from(unit.pane(), &b, facts, "done").await,
            code::TASK_CLOSED,
        );
        assert_eq!(answer.destination, None);
        assert_eq!(unit.ended(), 1);
        assert_eq!(unit.completed(), 0);
        assert_eq!(unit.route_count(), 0);
        assert!(super::report_tests::store_files(&unit.deck.store).is_empty());
    }
}

/// Scenario: An orchestration dispatch is paused after spawning its roles, and its orchestrator starts a delayed terminal report. A role closes before registration can finish, so the dispatch aborts; released afterwards, the report says unknown_task with the abandoned wording, saves nothing and never claims destination none.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_c_prime_an_aborted_publication_refuses_the_delayed_report() {
    let deck = Arc::new(deck());
    let caller = spawn_cat(&deck.registry, "r027cp-caller");
    let (reached, release) = deck.registry.pause_at_for_test("dispatch-publication");
    let dispatch = {
        let deck = Arc::clone(&deck);
        tokio::spawn(async move {
            dispatch_orchestration(&deck, "r027cp", "r027cp-caller", &caller).await
        })
    };
    reached.await.unwrap();
    let (pane, agent) = deck
        .registry
        .live_pane_agents()
        .into_iter()
        .find(|(_, agent)| {
            deck.registry
                .attest_facts(agent)
                .and_then(|f| f.seat)
                .is_some_and(|s| s.is_orchestrator)
        })
        .unwrap();
    let facts = deck.registry.attest_facts(&agent).unwrap();
    let (attested, release_report) = deck
        .registry
        .pause_at_for_test(crate::task_completion::PAUSE_TERMINAL_RESOLVED);
    let report = {
        let deck = Arc::clone(&deck);
        tokio::spawn(async move {
            crate::task_completion::complete_with_state(
                &AppState::default(),
                &deck.registry,
                CompletionRequest {
                    pane_id: pane,
                    mode: WorkDoneMode::Terminal,
                    report: "aborted report".to_string(),
                },
                &Provenance::Attested {
                    agent_id: agent,
                    facts,
                },
            )
            .await
        })
    };
    attested.await.unwrap();
    let worker = deck
        .registry
        .live_pane_agents()
        .into_iter()
        .find(|(_, agent)| {
            deck.registry
                .attest_facts(agent)
                .and_then(|f| f.seat)
                .is_some_and(|s| !s.is_orchestrator)
        })
        .unwrap()
        .1;
    deck.registry.close_agent(&worker).unwrap();
    release.send(()).unwrap();
    let Err(aborted) = dispatch.await else {
        panic!("the closed role must abort the dispatch");
    };
    assert!(
        aborted.is_panic(),
        "the test dispatch helper expects success, so abort is a panic"
    );
    release_report.send(()).unwrap();
    let answer = refused_with(report.await.unwrap(), code::UNKNOWN_TASK);
    assert!(
        answer.message.as_deref().unwrap().contains("abandoned"),
        "{answer:?}"
    );
    assert_eq!(answer.destination, None);
    assert!(super::report_tests::store_files(&deck.store).is_empty());
    assert!(inbox_of(&deck, "r027cp-caller").is_empty());
}

/// Start a report on its own task, held at `pause`.
async fn paused_report(
    unit: &Arc<Unit>,
    pause: &str,
    body: &'static str,
) -> (
    tokio::task::JoinHandle<Completion>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (reached, release) = unit.registry().pause_at_for_test(pause);
    let report = {
        let unit = Arc::clone(unit);
        tokio::spawn(async move { unit.report(body).await })
    };
    reached.await.unwrap();
    (report, release)
}

/// Scenario: A single unit's terminal report is paused after its save, and the unit's agent exits meanwhile. Released, the report reaches the caller once, the route is gone, the unit reads as completed, and no `dispatch ended` is posted.
#[spec("dispatch/return/013")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_013_a_terminal_report_paused_across_its_units_exit() {
    let unit = Arc::new(unit("r013", false).await);
    let (report, release) =
        paused_report(&unit, crate::task_completion::PAUSE_SAVED, "unit done").await;
    kill_child(unit.registry(), unit.agent());
    eventually("the route ends", || unit.route_count() == 0).await;
    release.send(()).unwrap();
    accepted(report.await.unwrap());
    assert_eq!(unit.completed(), 1);
    assert_eq!(unit.ended(), 0);
    assert!(matches!(
        unit_entry(&unit.deck, &unit.unit_id()),
        Some(UnitEntry::Completed { .. })
    ));
}

/// Scenario: As `013`, against a deliberate close of the unit's pane: the report reaches the caller once, and no `dispatch ended` is posted.
#[spec("dispatch/return/014")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_014_a_terminal_report_paused_across_its_units_close() {
    let unit = Arc::new(unit("r014", false).await);
    let (report, release) =
        paused_report(&unit, crate::task_completion::PAUSE_SAVED, "unit done").await;
    unit.close();
    release.send(()).unwrap();
    accepted(report.await.unwrap());
    assert_eq!(unit.completed(), 1);
    assert_eq!(unit.ended(), 0);
}

/// Scenario: As `013`, against a respawn of the unit's agent: the report reaches the caller once, and the successor's identical terminal report is answered with the first result.
#[spec("dispatch/return/015")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_015_a_terminal_report_paused_across_a_respawn() {
    let unit = Arc::new(unit("r015", false).await);
    let (report, release) =
        paused_report(&unit, crate::task_completion::PAUSE_SAVED, "unit done").await;
    unit.registry()
        .respawn_agent_for_pane(unit.pane(), "cat")
        .await
        .expect("respawn");
    release.send(()).unwrap();
    let first = accepted(report.await.unwrap());
    assert_eq!(unit.completed(), 1);
    let successor = unit.registry().pane_current_agent_id(unit.pane()).unwrap();
    let again = reply(
        unit.report_from(unit.pane(), &successor, unit.facts(&successor), "unit done")
            .await,
    );
    assert_eq!(again.disposition, WorkDoneDisposition::AlreadyAccepted);
    assert_eq!(again.report_id, first.report_id);
}

/// Scenario: A terminal report's save fails after the unit's agent exited while it was paused. The caller gets one `dispatch ended`, the unit is not marked complete, and no saved file is left.
#[spec("dispatch/return/016")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_016_a_failed_terminal_save_after_the_units_exit_ends_the_unit() {
    let unit = Arc::new(unit("r016", false).await);
    unit.registry()
        .inject_fault_for_test(crate::task_completion::FAULT_FAIL_SAVE, TestFault::Fail);
    let (report, release) =
        paused_report(&unit, crate::task_completion::PAUSE_SAVED, "unit done").await;
    kill_child(unit.registry(), unit.agent());
    eventually("the route ends", || unit.route_count() == 0).await;
    release.send(()).unwrap();
    refused_with(report.await.unwrap(), code::REPORT_SAVE_FAILED);
    eventually("one dispatch ended", || unit.ended() == 1).await;
    assert_eq!(unit.completed(), 0);
    assert_eq!(
        unit_entry(&unit.deck, &unit.unit_id()),
        Some(UnitEntry::Ended {
            how: EndedHow::Exited
        })
    );
    assert!(
        unit.registry()
            .dispatched_units()
            .get(&unit.unit_id())
            .is_none_or(|u| u.completed_at_ms.is_none())
    );
    assert_eq!(
        std::fs::read_dir(unit.deck.store.dir()).unwrap().count(),
        0,
        "no saved report"
    );
}

/// Scenario: A unit reports, then sends a different terminal report. The repeat is answered `differs`, exit 1, nothing is delivered, and the first report stands.
#[spec("dispatch/return/019")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_019_a_differing_terminal_repeat_is_not_delivered() {
    let unit = unit("r019", false).await;
    accepted(unit.report("first").await);
    let differs = reply(unit.report("second").await);
    assert_eq!(differs.disposition, WorkDoneDisposition::AlreadyAccepted);
    assert!(differs.differs);
    let answer = crate::work_done_text::cli_answer(&differs, None, true);
    assert!(!answer.success);
    assert_eq!(unit.completed(), 1);
}

/// Scenario: An older dot-agent-deck's `work-done --done`, through the legacy verb's adapter: the unit's first report is acknowledged and delivered. The same body again is acknowledged and changes nothing. A different body is refused, saying the first report stands and this one was not delivered, and it changes nothing: the caller still holds only the first report, and no file is saved.
#[spec("dispatch/return/019")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_019_the_legacy_verb_refuses_a_differing_repeat() {
    let unit = unit("r019l", false).await;
    let legacy = |body: &str| {
        let provenance = Provenance::Attested {
            agent_id: unit.agent().to_string(),
            facts: unit.facts(unit.agent()),
        };
        let signal = crate::event::WorkDoneSignal {
            pane_id: unit.pane().to_string(),
            task: body.to_string(),
            done: true,
            timestamp: chrono::Utc::now(),
            token: None,
        };
        let registry = Arc::clone(unit.registry());
        async move {
            let state = tokio::sync::RwLock::new(AppState::default());
            crate::task_completion::complete_legacy(&state, &registry, signal, &provenance)
                .await
                .to_signal_ack()
                .expect("an answer")
        }
    };
    let delivered = |needle: &str| {
        unit.registry()
            .inbox()
            .rendered(&unit.caller_pane)
            .into_iter()
            .filter(|delivery| delivery.text.contains(needle))
            .count()
    };
    let first = legacy("LEGACY-019-FIRST").await;
    assert!(first.accepted, "{first:?}");
    assert_eq!(unit.completed(), 1);
    assert_eq!(delivered("LEGACY-019-FIRST"), 1);
    let files = super::report_tests::store_files(&unit.deck.store);
    let caller = inbox_of(&unit.deck, &unit.caller_pane);

    let same = legacy("LEGACY-019-FIRST").await;
    assert!(same.accepted, "an identical repeat succeeds: {same:?}");
    let differs = legacy("LEGACY-019-SECOND").await;
    assert!(
        !differs.accepted,
        "a differing repeat is refused: {differs:?}"
    );
    assert_eq!(differs.reason.as_deref(), Some(code::REPORT_DIFFERS));
    assert!(
        differs
            .error
            .as_deref()
            .is_some_and(|e| e.contains("NOT delivered") && e.contains("first one stands")),
        "{differs:?}"
    );
    assert_eq!(inbox_of(&unit.deck, &unit.caller_pane), caller);
    assert_eq!(delivered("LEGACY-019-SECOND"), 0);
    assert_eq!(delivered("LEGACY-019-FIRST"), 1);
    assert_eq!(super::report_tests::store_files(&unit.deck.store), files);
    assert_eq!(unit.completed(), 1);
}

/// Scenario: A terminal report is paused after its role-map snapshot and before it takes the routes; the unit's agent exits, and in a second run the unit is closed. Released, it is refused `task_closed` with the ended wording; exactly one `dispatch ended` is posted; no claim, no saved report and no mark are left.
#[spec("dispatch/return/020")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_020_termination_before_the_claim_answers_task_closed() {
    for close in [false, true] {
        let unit = Arc::new(unit(&format!("r020-{close}"), false).await);
        let (report, release) = paused_report(
            &unit,
            crate::task_completion::PAUSE_TERMINAL_RESOLVED,
            "done",
        )
        .await;
        if close {
            unit.close();
        } else {
            kill_child(unit.registry(), unit.agent());
        }
        eventually("the unit ends", || {
            unit_entry(&unit.deck, &unit.unit_id()).is_some()
        })
        .await;
        release.send(()).unwrap();
        let refusal = refused_with(report.await.unwrap(), code::TASK_CLOSED);
        assert!(refusal.message.unwrap().contains("already ended"));
        eventually("one dispatch ended", || unit.ended() == 1).await;
        assert_eq!(unit.completed(), 0);
        assert_eq!(
            unit.registry()
                .inbox()
                .read_reports(|r| r.claims.len())
                .unwrap(),
            0
        );
        assert_eq!(
            std::fs::read_dir(unit.deck.store.dir()).unwrap().count(),
            0,
            "no saved report"
        );
    }
}

/// Scenario: A terminal report holds its claim (paused after its save) when the unit's agent exits, and in a second run when the unit is closed: the claim is marked with the unit gone; released, the report reaches the caller once, with no `dispatch ended`, and the route is gone once.
#[spec("dispatch/return/021")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_021_a_claim_before_termination_still_reaches_the_caller() {
    for close in [false, true] {
        let unit = Arc::new(unit(&format!("r021-{close}"), false).await);
        let (report, release) =
            paused_report(&unit, crate::task_completion::PAUSE_SAVED, "done").await;
        if close {
            unit.close();
        } else {
            kill_child(unit.registry(), unit.agent());
        }
        eventually("the claim learns its unit ended", || {
            unit.registry()
                .inbox()
                .read_reports(|r| {
                    r.claims
                        .of_generation(unit.pane(), unit.agent())
                        .is_some_and(|id| r.claims.get(id).is_some_and(|c| c.unit_gone.is_some()))
                })
                .unwrap()
        })
        .await;
        release.send(()).unwrap();
        accepted(report.await.unwrap());
        assert_eq!(unit.completed(), 1);
        assert_eq!(unit.ended(), 0);
        assert_eq!(unit.route_count(), 0);
    }
}

/// Scenario: A single unit and an orchestration unit each report terminal and are accepted. An identical repeat is answered with the first report id, exit 0; a differing one `differs`, exit 1; after the unit is closed the repeat is still answered through the index; a generation that names no unit is refused `incompatible_mode`. Nothing new is posted, saved or marked.
#[spec("dispatch/return/022")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_022_terminal_retries_after_the_route_was_consumed() {
    for orchestration in [false, true] {
        let unit = unit(&format!("r022-{orchestration}"), orchestration).await;
        let first = accepted(unit.report("done").await);
        assert_eq!(unit.route_count(), 0, "the route was consumed");
        let again = reply(unit.report("done").await);
        assert_eq!(
            (again.disposition, again.report_id.clone(), again.differs),
            (
                WorkDoneDisposition::AlreadyAccepted,
                first.report_id.clone(),
                false
            )
        );
        let differs = reply(unit.report("other").await);
        assert!(differs.differs);
        unit.close();
        let after_close = reply(
            unit.report_from(unit.pane(), unit.agent(), unit.facts(unit.agent()), "done")
                .await,
        );
        assert_eq!(
            after_close.disposition,
            WorkDoneDisposition::AlreadyAccepted
        );
        let stranger_pane = format!("r022-{orchestration}-stranger");
        let stranger = spawn_cat(unit.registry(), &stranger_pane);
        refused_with(
            unit.report_from(&stranger_pane, &stranger, unit.facts(&stranger), "done")
                .await,
            code::INCOMPATIBLE_MODE,
        );
        assert_eq!(unit.completed(), 1);
    }
}

/// Scenario: A forced close lands before a terminal report's unit mark: the mark answers that the unit ended first, the caller gets the report once and no `dispatch ended`. A close after the mark (paused there): the caller gets the report once.
#[spec("dispatch/return/025")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_025_a_close_against_the_unit_mark() {
    // Before the mark.
    let unit = Arc::new(unit("r025a", false).await);
    let (report, release) = paused_report(&unit, crate::task_completion::PAUSE_SAVED, "done").await;
    let _ = unit.registry().dispatched_units().end(
        &unit.unit_id(),
        crate::dispatched_units::EndReason::ClosedByVerb,
        chrono::Utc::now().timestamp_millis(),
    );
    unit.close();
    release.send(()).unwrap();
    accepted(report.await.unwrap());
    assert_eq!(unit.completed(), 1);
    assert_eq!(unit.ended(), 0);
    assert!(matches!(
        unit_entry(&unit.deck, &unit.unit_id()),
        Some(UnitEntry::Completed {
            unit_mark: UnitMark::UnitEndedFirst,
            ..
        })
    ));

    // After the mark.
    let unit = Arc::new(self::unit("r025b", false).await);
    let (report, release) =
        paused_report(&unit, crate::task_completion::PAUSE_MARKED, "done").await;
    assert!(
        unit.registry()
            .dispatched_units()
            .get(&unit.unit_id())
            .is_some_and(|u| u.completed_at_ms.is_some()),
        "the unit reads as reported"
    );
    unit.close();
    release.send(()).unwrap();
    accepted(report.await.unwrap());
    assert_eq!(unit.completed(), 1);
    assert_eq!(unit.ended(), 0);
}

/// Scenario: (e1) while a unit's respawn is paused before its publish, the successor sends a terminal report: `report_pending`, nothing consumed; released, the same report is accepted to the original caller once. (e3) the orchestrator of an orchestration started directly reports during and after its own respawn: `destination: none` both times.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_e1_e3_reports_during_a_replacement() {
    let unit = Arc::new(unit("r026e1", false).await);
    let (reached, release) = unit
        .registry()
        .pause_at_sync_for_test(PAUSE_RESPAWN_BEFORE_PUBLISH);
    let respawn = {
        let unit = Arc::clone(&unit);
        tokio::spawn(async move {
            unit.registry()
                .respawn_agent_for_pane(&unit.pane, "cat")
                .await
        })
    };
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    let b = unit.registry().pane_current_agent_id(unit.pane()).unwrap();
    refused_with(
        unit.report_from(unit.pane(), &b, unit.facts(&b), "done")
            .await,
        code::REPORT_PENDING,
    );
    assert_eq!(
        std::fs::read_dir(unit.deck.store.dir()).unwrap().count(),
        0,
        "no saved report"
    );
    release.send(()).unwrap();
    respawn.await.unwrap().unwrap();
    accepted(
        unit.report_from(unit.pane(), &b, unit.facts(&b), "done")
            .await,
    );
    assert_eq!(unit.completed(), 1);

    // (e3) a directly started orchestration's orchestrator.
    let deck = deck();
    let pane = "r026e3-orch";
    let orch = deck
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: "direct".to_string(),
                role_index: 0,
                role_name: "orchestrator".to_string(),
                is_start_role: true,
                orchestration_cwd: None,
                display_title: None,
                orchestration_id: Some("r026e3-instance".to_string()),
            }),
            origin: Some(InstanceOrigin::Direct),
            ..SpawnOptions::default()
        })
        .unwrap();
    let report = |agent: String| {
        let registry = Arc::clone(&deck.registry);
        async move {
            crate::task_completion::complete_with_state(
                &AppState::default(),
                &registry,
                CompletionRequest {
                    pane_id: pane.to_string(),
                    mode: WorkDoneMode::Terminal,
                    report: "orchestration done".to_string(),
                },
                &Provenance::Attested {
                    facts: registry.attest_facts(&agent).unwrap_or_default(),
                    agent_id: agent,
                },
            )
            .await
        }
    };
    let none = accepted(report(orch.clone()).await);
    assert_eq!(none.destination, Some(Destination::None));
    deck.registry
        .respawn_agent_for_pane(pane, "cat")
        .await
        .unwrap();
    let successor = deck.registry.pane_current_agent_id(pane).unwrap();
    let none = accepted(report(successor).await);
    assert_eq!(none.destination, Some(Destination::None));
    assert!(rows(&deck).is_empty());
}

/// Scenario: (a) a terminal report paused after its snapshot, released after the unit was closed: `task_closed`, never `incompatible_mode` and never `destination: none`, for a single and an orchestration unit. (c) the dispatch paused before registering its unit: the terminal generation's report reads `report_pending`; released and registered, the next report is accepted.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_a_c_d_a_unit_that_ends_or_registers_around_its_report() {
    for orchestration in [false, true] {
        let unit = Arc::new(unit(&format!("r027a-{orchestration}"), orchestration).await);
        let (report, release) = paused_report(
            &unit,
            crate::task_completion::PAUSE_TERMINAL_RESOLVED,
            "done",
        )
        .await;
        unit.close();
        eventually("the unit ends", || {
            unit_entry(&unit.deck, &unit.unit_id()).is_some()
        })
        .await;
        release.send(()).unwrap();
        refused_with(report.await.unwrap(), code::TASK_CLOSED);
        assert_eq!(unit.completed(), 0);
    }

    // (c) the registration window.
    let deck = Arc::new(deck());
    let caller = spawn_cat(&deck.registry, "r027c-caller");
    let (reached, release) = deck.registry.pause_at_for_test("dispatch-publication");
    let dispatch = {
        let deck = Arc::clone(&deck);
        let caller = caller.clone();
        tokio::spawn(async move { dispatch_single(&deck, "r027c", "r027c-caller", &caller).await })
    };
    reached.await.unwrap();
    let (pane, agent) = deck
        .registry
        .live_pane_agents()
        .into_iter()
        .find(|(pane, _)| pane.starts_with("sched-dispatch-r027c"))
        .expect("the unit's agent is spawned");
    let terminal = |deck: Arc<Deck>, pane: String, agent: String| async move {
        crate::task_completion::complete_with_state(
            &AppState::default(),
            &deck.registry,
            CompletionRequest {
                pane_id: pane,
                mode: WorkDoneMode::Terminal,
                report: "done".to_string(),
            },
            &Provenance::Attested {
                facts: deck.registry.attest_facts(&agent).unwrap_or_default(),
                agent_id: agent,
            },
        )
        .await
    };
    refused_with(
        terminal(Arc::clone(&deck), pane.clone(), agent.clone()).await,
        code::REPORT_PENDING,
    );
    release.send(()).unwrap();
    dispatch.await.unwrap();
    accepted(terminal(Arc::clone(&deck), pane, agent).await);
    assert_eq!(
        inbox_of(&deck, "r027c-caller")
            .iter()
            .filter(|(s, _)| s == "dispatch completed")
            .count(),
        1
    );
}

/// Scenario: (c) for an orchestration: the dispatch is paused after its roles are spawned and before its unit is registered, so an `Instance` publication covers its orchestrator. The orchestrator's terminal report reads `report_pending`, never `destination: none`, and nothing is saved, claimed or posted; released and registered, its report is accepted to the caller once.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_c_an_orchestrators_report_during_registration_is_pending() {
    let deck = Arc::new(deck());
    let caller = spawn_cat(&deck.registry, "r027co-caller");
    let (reached, release) = deck.registry.pause_at_for_test("dispatch-publication");
    let dispatch = {
        let deck = Arc::clone(&deck);
        let caller = caller.clone();
        tokio::spawn(async move {
            dispatch_orchestration(&deck, "r027co", "r027co-caller", &caller).await
        })
    };
    reached.await.unwrap();
    let (pane, orchestrator) = deck
        .registry
        .live_pane_agents()
        .into_iter()
        .find(|(_, agent)| {
            deck.registry
                .attest_facts(agent)
                .and_then(|facts| facts.seat)
                .is_some_and(|seat| seat.is_orchestrator)
        })
        .expect("the orchestrator is spawned");
    let terminal = |deck: Arc<Deck>, pane: String, agent: String| async move {
        crate::task_completion::complete_with_state(
            &AppState::default(),
            &deck.registry,
            CompletionRequest {
                pane_id: pane,
                mode: WorkDoneMode::Terminal,
                report: "orchestration done".to_string(),
            },
            &Provenance::Attested {
                facts: deck.registry.attest_facts(&agent).unwrap_or_default(),
                agent_id: agent,
            },
        )
        .await
    };
    let pending = refused_with(
        terminal(Arc::clone(&deck), pane.clone(), orchestrator.clone()).await,
        code::REPORT_PENDING,
    );
    assert_eq!(pending.destination, None);
    assert_eq!(super::report_tests::store_files(&deck.store).len(), 0);
    assert_eq!(
        deck.registry
            .inbox()
            .read_reports(|r| r.claims.len())
            .unwrap(),
        0
    );
    assert!(inbox_of(&deck, "r027co-caller").is_empty());
    release.send(()).unwrap();
    dispatch.await.unwrap();
    let answer = accepted(terminal(Arc::clone(&deck), pane, orchestrator).await);
    assert_eq!(answer.destination, Some(Destination::Inbox));
    assert_eq!(
        inbox_of(&deck, "r027co-caller")
            .iter()
            .filter(|(s, _)| s == "dispatch completed")
            .count(),
        1
    );
}

/// Scenario: (e2) a delayed predecessor, for a single and an orchestration unit: A's terminal report attests and is held before it reads the routes, and A's pane is respawned to B. Released while the replacement is still being published, it reads `report_pending`; a second held request from A, released after the publish, reads `task_closed` with the replaced wording. Neither consumes anything: no claim, file, mark or post. B then reports and is accepted once.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_e2_a_delayed_predecessor_is_refused() {
    for orchestration in [false, true] {
        let unit = Arc::new(unit(&format!("r026e2-{orchestration}"), orchestration).await);
        let a = unit.agent().to_string();
        let facts_a = unit.facts(&a);
        let held_a = |unit: &Arc<Unit>| {
            let (reached, release) = unit
                .registry()
                .pause_at_for_test(crate::task_completion::PAUSE_TERMINAL_RESOLVED);
            let report = {
                let unit = Arc::clone(unit);
                let (a, facts) = (a.clone(), facts_a.clone());
                tokio::spawn(async move { unit.report_from(&unit.pane, &a, facts, "A done").await })
            };
            (reached, release, report)
        };
        let nothing_consumed = |unit: &Unit| {
            assert_eq!(
                unit.registry()
                    .inbox()
                    .read_reports(|r| r.claims.len())
                    .unwrap(),
                0
            );
            assert_eq!(super::report_tests::store_files(&unit.deck.store).len(), 0);
            assert_eq!(unit.completed(), 0);
            assert!(
                unit.registry()
                    .dispatched_units()
                    .get(&unit.unit_id())
                    .is_some_and(|u| u.completed_at_ms.is_none())
            );
        };
        let (reached, release, during) = held_a(&unit);
        reached.await.unwrap();
        let (published, publish) = unit
            .registry()
            .pause_at_sync_for_test(PAUSE_RESPAWN_BEFORE_PUBLISH);
        let respawn = {
            let unit = Arc::clone(&unit);
            tokio::spawn(async move {
                unit.registry()
                    .respawn_agent_for_pane(&unit.pane, "cat")
                    .await
            })
        };
        tokio::task::spawn_blocking(move || published.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
        refused_with(during.await.unwrap(), code::REPORT_PENDING);
        nothing_consumed(&unit);

        let (reached, release, after) = held_a(&unit);
        reached.await.unwrap();
        publish.send(()).unwrap();
        respawn.await.unwrap().unwrap();
        release.send(()).unwrap();
        let refusal = refused_with(after.await.unwrap(), code::TASK_CLOSED);
        assert!(
            refusal
                .message
                .as_deref()
                .is_some_and(|m| m.contains("replaced by a restart")),
            "{refusal:?}"
        );
        nothing_consumed(&unit);

        let b = unit.registry().pane_current_agent_id(unit.pane()).unwrap();
        assert_ne!(b, a);
        accepted(
            unit.report_from(unit.pane(), &b, unit.facts(&b), "B done")
                .await,
        );
        assert_eq!(unit.completed(), 1);
    }
}

/// Scenario: (h) the R24 schedule with a delayed request, for a single and an orchestration unit. The dispatch is paused before its unit is registered, so its terminal generation G is spawned and attests with no unit. G's terminal report attests and is held before it reads the role map. The dispatch is released and registers U; U is closed and ends, with one `dispatch ended`; U's entry and index are evicted; and a directly started orchestration's orchestrator takes G's pane, so the role map now names a direct orchestrator there. Released, G's report reads that role map (the probe shows it) and is refused — `unknown_task` for the orchestration, `incompatible_mode` for the single unit — never `destination: none`: nothing is saved, claimed, marked or posted, no second `dispatch ended` is sent and the successor's inbox stays empty. The direct successor's own report then reads `destination: none`.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_h_a_delayed_report_across_registration_eviction_and_pane_reuse() {
    // SAFETY: a nextest process runs one test.
    unsafe { std::env::set_var(crate::report_recovery::ENV_TOMBSTONES_MAX, "1") };
    for orchestration in [false, true] {
        let tag = format!("r027h{}", u8::from(orchestration));
        let caller_pane = format!("{tag}-caller");
        let deck = Arc::new(deck());
        let caller = spawn_cat(&deck.registry, &caller_pane);
        let (reached, release_dispatch) = deck.registry.pause_at_for_test("dispatch-publication");
        let dispatch = {
            let (deck, tag, caller_pane, caller) = (
                Arc::clone(&deck),
                tag.clone(),
                caller_pane.clone(),
                caller.clone(),
            );
            tokio::spawn(async move {
                if orchestration {
                    dispatch_orchestration(&deck, &tag, &caller_pane, &caller).await
                } else {
                    dispatch_single(&deck, &tag, &caller_pane, &caller).await
                }
            })
        };
        reached.await.unwrap();
        // G: the single unit's agent, or the orchestration's orchestrator.
        let (pane, g) = deck
            .registry
            .live_pane_agents()
            .into_iter()
            .find(|(pane, agent)| {
                pane.starts_with(&format!("sched-dispatch-{tag}"))
                    && (!orchestration
                        || deck
                            .registry
                            .attest_facts(agent)
                            .and_then(|facts| facts.seat)
                            .is_some_and(|seat| seat.is_orchestrator))
            })
            .expect("G is spawned");
        let facts = deck.registry.attest_facts(&g).unwrap();
        assert_eq!(facts.unit, None, "G attests with no unit");
        let state = Arc::new(tokio::sync::RwLock::new(AppState::default()));
        let (attested, release_report) = deck
            .registry
            .pause_at_for_test(crate::task_completion::PAUSE_ATTESTED);
        let report = {
            let (deck, state, pane, g) = (
                Arc::clone(&deck),
                Arc::clone(&state),
                pane.clone(),
                g.clone(),
            );
            tokio::spawn(async move {
                crate::task_completion::complete(
                    &state,
                    &deck.registry,
                    CompletionRequest {
                        pane_id: pane,
                        mode: WorkDoneMode::Terminal,
                        report: "G done".to_string(),
                    },
                    &Provenance::Attested { agent_id: g, facts },
                )
                .await
            })
        };
        attested.await.unwrap();
        release_dispatch.send(()).unwrap();
        let handle = dispatch.await.unwrap();
        let unit_id = handle.unit_id.clone().unwrap();
        // U is closed, and ends.
        drop(deck.registry.begin_pane_close(&pane));
        let _ = deck.registry.close_agent(&g);
        drop(deck.registry.finish_pane_close(&pane, true));
        eventually("U ends", || {
            inbox_of(&deck, &caller_pane)
                .iter()
                .any(|(s, _)| s == "dispatch ended")
        })
        .await;
        // U's entry and index are evicted.
        let others: Vec<(String, String, String)> = (0..32)
            .map(|n| {
                (
                    format!("{tag}-p{n}"),
                    format!("gone-{n}"),
                    format!("u-{tag}-{n}"),
                )
            })
            .collect();
        let refs: Vec<(&str, &str, &str)> = others
            .iter()
            .map(|(p, a, u)| (p.as_str(), a.as_str(), u.as_str()))
            .collect();
        deck.registry
            .inbox()
            .insert_ended_units_for_test(deck.registry.as_ref(), &refs);
        assert!(unit_entry(&deck, &unit_id).is_none(), "U is evicted");
        // A direct orchestration's orchestrator takes G's pane.
        let direct_instance = format!("{tag}-direct");
        let successor = deck
            .registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.clone())],
                tab_membership: Some(TabMembership::Orchestration {
                    name: "direct".to_string(),
                    role_index: 0,
                    role_name: "orchestrator".to_string(),
                    is_start_role: true,
                    orchestration_cwd: None,
                    display_title: None,
                    orchestration_id: Some(direct_instance.clone()),
                }),
                origin: Some(InstanceOrigin::Direct),
                ..SpawnOptions::default()
            })
            .expect("the direct orchestrator takes the pane");
        {
            let mut st = state.write().await;
            st.pane_role_map
                .insert(pane.clone(), "orchestrator".to_string());
            st.orchestrator_pane_ids.insert(pane.clone());
            st.pane_orchestration_map.insert(
                pane.clone(),
                crate::state::OrchestrationIdentity {
                    id: direct_instance.clone(),
                    name: "direct".to_string(),
                },
            );
        }
        release_report.send(()).unwrap();
        let answer = report.await.unwrap();
        let seen = crate::task_completion::snapshot_probe::seen(&pane).unwrap();
        assert!(
            seen.contains("is_orchestrator: true") && seen.contains(&direct_instance),
            "the report read the direct successor's role map: {seen}"
        );
        let refusal = refused_with(
            answer,
            if orchestration {
                code::UNKNOWN_TASK
            } else {
                code::INCOMPATIBLE_MODE
            },
        );
        assert_ne!(refusal.destination, Some(Destination::None));
        assert_eq!(super::report_tests::store_files(&deck.store).len(), 0);
        assert_eq!(
            deck.registry
                .inbox()
                .read_reports(|r| r.claims.len())
                .unwrap(),
            0
        );
        let caller_inbox = inbox_of(&deck, &caller_pane);
        assert_eq!(
            caller_inbox
                .iter()
                .filter(|(s, _)| s == "dispatch ended")
                .count(),
            1,
            "no second dispatch ended"
        );
        assert!(!caller_inbox.iter().any(|(s, _)| s == "dispatch completed"));
        assert!(
            deck.registry
                .dispatched_units()
                .get(&unit_id)
                .is_none_or(|u| u.completed_at_ms.is_none()),
            "no mark"
        );
        let for_successor: Vec<String> = deck
            .registry
            .inbox()
            .list(&pane)
            .into_iter()
            .filter(|m| {
                m.recipient.as_ref().and_then(|r| r.agent_id.as_deref()) == Some(successor.as_str())
            })
            .map(|m| m.subject)
            .collect();
        assert!(
            for_successor.is_empty(),
            "nothing for the successor: {for_successor:?}"
        );
        // The direct successor's own report.
        let none = accepted(
            crate::task_completion::complete_with_state(
                &AppState::default(),
                &deck.registry,
                CompletionRequest {
                    pane_id: pane.clone(),
                    mode: WorkDoneMode::Terminal,
                    report: "direct done".to_string(),
                },
                &Provenance::Attested {
                    facts: deck.registry.attest_facts(&successor).unwrap(),
                    agent_id: successor.clone(),
                },
            )
            .await,
        );
        assert_eq!(none.destination, Some(Destination::None));
    }
}

/// Scenario: (j) a single unit's agent A, spawned with no orchestration seat, sends its terminal report, which attests and is held before it reads the role map. A exits with its exit cleanup held, so its route stays; B starts on A's pane as a member of another orchestration instance, and the role map now names that instance there. Released, A's report is accepted for A's unit, and the retained report names no instance: never B's.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_j_a_seatless_terminal_report_never_takes_its_successors_instance() {
    let unit = unit("r027j", false).await;
    let a = unit.agent().to_string();
    let pane = unit.pane().to_string();
    let facts = unit.facts(&a);
    assert!(facts.seat.is_none(), "A has no orchestration seat");
    let state = Arc::new(tokio::sync::RwLock::new(AppState::default()));
    let held = unit.registry().hold_exit_cleanup_for_test(&a);
    let (attested, release) = unit
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_ATTESTED);
    let report = {
        let (registry, state, pane, a) = (
            Arc::clone(unit.registry()),
            Arc::clone(&state),
            pane.clone(),
            a.clone(),
        );
        tokio::spawn(async move {
            crate::task_completion::complete(
                &state,
                &registry,
                CompletionRequest {
                    pane_id: pane,
                    mode: WorkDoneMode::Terminal,
                    report: "A-027J-REPORT".to_string(),
                },
                &Provenance::Attested { agent_id: a, facts },
            )
            .await
        })
    };
    attested.await.unwrap();
    kill_child(unit.registry(), &a);
    let successor_instance = "r027j-successor-instance".to_string();
    let mut b = None;
    eventually("B starts on A's pane", || {
        b = unit
            .registry()
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.clone())],
                tab_membership: Some(TabMembership::Orchestration {
                    name: "other".to_string(),
                    role_index: 1,
                    role_name: "coder".to_string(),
                    is_start_role: false,
                    orchestration_cwd: None,
                    display_title: None,
                    orchestration_id: Some(successor_instance.clone()),
                }),
                origin: Some(InstanceOrigin::Direct),
                ..SpawnOptions::default()
            })
            .ok();
        b.is_some()
    })
    .await;
    {
        let mut st = state.write().await;
        st.pane_role_map.insert(pane.clone(), "coder".to_string());
        st.pane_orchestration_map.insert(
            pane.clone(),
            crate::state::OrchestrationIdentity {
                id: successor_instance.clone(),
                name: "other".to_string(),
            },
        );
    }
    release.send(()).unwrap();
    let answer = accepted(report.await.unwrap());
    let seen = crate::task_completion::snapshot_probe::seen(&pane).unwrap();
    assert!(
        seen.contains(&successor_instance),
        "the report read the successor's role map: {seen}"
    );
    let file = std::fs::File::open(
        unit.deck
            .store
            .dir()
            .join(crate::retained_store::own_name(&answer.report_id.unwrap())),
    )
    .unwrap();
    let envelope = crate::retained_store::read_attribution(file).expect("an envelope");
    assert_eq!(envelope.instance, None, "never the successor's instance");
    assert_eq!(envelope.producer_agent_id, a);
    assert_eq!(unit.completed(), 1);
    drop(held);
    let _ = unit.registry().close_agent(b.as_deref().unwrap());
}

/// Scenario: (i) an orchestrator seat whose instance's generations disagree on how it started reports terminal with no unit evidence: refused `incompatible_mode`, never `destination: none`.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_i_a_conflicting_origin_is_never_read_as_direct() {
    let unit = unit("r027ci", true).await;
    let instance = unit.orchestration_id.clone().unwrap();
    let pane = "r027ci-intruder";
    let intruder = unit
        .registry()
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: "r027ci".to_string(),
                role_index: 0,
                role_name: "orchestrator".to_string(),
                is_start_role: true,
                orchestration_cwd: None,
                display_title: None,
                orchestration_id: Some(instance),
            }),
            origin: Some(InstanceOrigin::Direct),
            ..SpawnOptions::default()
        })
        .unwrap();
    let facts = unit.facts(&intruder);
    assert_eq!(
        facts.seat.as_ref().and_then(|s| s.origin),
        Some(InstanceOrigin::Conflict)
    );
    refused_with(
        unit.report_from(pane, &intruder, facts, "done").await,
        code::INCOMPATIBLE_MODE,
    );
}

/// Scenario: A terminal report is paused after its unit mark, so the unit reads as reported, and the caller closes the unit without force; then the save task panics before the commit. The supervisor rolls it forward: the caller has exactly one report, no `dispatch ended`, and the unit is completed. The apply arms: a panic after `Applying` and before the post leaves no report in the caller's inbox and the connection unconfirmed; a panic after the post leaves the one inserted report.
#[spec("orchestration/work-done/054")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_054_a_close_acting_on_the_mark_then_a_panic() {
    let unit = Arc::new(unit("wd054", false).await);
    unit.registry().inject_fault_for_test(
        crate::task_completion::FAULT_MARKED,
        TestFault::Panic("injected-054"),
    );
    let (report, release) =
        paused_report(&unit, crate::task_completion::PAUSE_MARKED, "done").await;
    unit.close();
    release.send(()).unwrap();
    accepted(report.await.unwrap());
    assert_eq!(unit.completed(), 1);
    assert_eq!(unit.ended(), 0);
    assert!(matches!(
        unit_entry(&unit.deck, &unit.unit_id()),
        Some(UnitEntry::Completed { .. })
    ));

    for after_post in [false, true] {
        let unit = self::unit(&format!("wd054-apply-{after_post}"), false).await;
        let seam = if after_post {
            crate::inbox_hub::claim_seams::APPLY_AFTER_POST
        } else {
            crate::inbox_hub::claim_seams::APPLY_BEFORE_POST
        };
        crate::inbox_hub::claim_seams::on(seam, unit.pane(), || {
            std::panic::panic_any("injected-054-apply")
        });
        assert_eq!(unit.report("done").await, Completion::Unconfirmed);
        assert_eq!(unit.completed(), usize::from(after_post));
        assert_eq!(unit.ended(), 0);
        assert!(
            unit.registry()
                .dispatched_units()
                .get(&unit.unit_id())
                .is_some_and(|u| u.completed_at_ms.is_some()),
            "the unit reads as reported"
        );
        assert_eq!(std::fs::read_dir(unit.deck.store.dir()).unwrap().count(), 1);
        std::mem::forget(unit);
    }
}

/// Scenario: (j) the retained store's lock is poisoned while a terminal claim is marked: the commit writes nothing and the connection reads no answer; the claim, the caller's inbox, the unit mark and the saved file are as they were; a repeat reads `deck_unavailable`.
#[spec("orchestration/work-done/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_053_j_an_unavailable_store_before_the_commit_writes_nothing() {
    let unit = Arc::new(unit("wd053j", false).await);
    let (report, release) =
        paused_report(&unit, crate::task_completion::PAUSE_MARKED, "done").await;
    // Everything the commit could change, read whole before and after.
    let state = |unit: &Unit| {
        let hub = unit.registry().inbox();
        (
            hub.reserved_replies(),
            hub.read_reports(|r| {
                (
                    r.claims.len(),
                    r.claims
                        .iter()
                        .map(|c| {
                            format!(
                                "{:?} {} {:?} {} {}",
                                c.phase,
                                c.prepared.is_some(),
                                c.unit_mark,
                                c.room,
                                c.reserved_bytes
                            )
                        })
                        .collect::<Vec<_>>(),
                    format!("{:?}", r.units.get(&unit.unit_id())),
                    r.accepted.len(),
                    r.outcomes.rows().len(),
                )
            })
            .unwrap(),
            inbox_of(&unit.deck, &unit.caller_pane),
            unit.route_count(),
            super::report_tests::store_files(&unit.deck.store),
            unit.registry()
                .dispatched_units()
                .get(&unit.unit_id())
                .map(|u| u.completed_at_ms),
        )
    };
    let before = state(&unit);
    unit.deck.store.poison_for_test();
    release.send(()).unwrap();
    assert_eq!(report.await.unwrap(), Completion::Unconfirmed);
    assert_eq!(unit.completed(), 0);
    assert_eq!(state(&unit), before, "the commit wrote nothing");
    assert_eq!(before.1.0, 1, "the claim stays where it was");
    refused_with(unit.report("done").await, code::DECK_UNAVAILABLE);
    std::mem::forget(Arc::try_unwrap(unit).ok());
}

/// Scenario: The routes' lock is poisoned by a panic on another thread while the deck's report state stays whole. The unit's terminal report is answered `deck_unavailable` on its own connection, with no panic in the reporting task, and nothing is claimed, saved, marked or posted.
#[spec("orchestration/work-done/053")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_053_k_poisoned_routes_refuse_a_terminal_report() {
    let unit = Arc::new(unit("wd053k", false).await);
    let registry = Arc::clone(unit.registry());
    let _ = std::thread::spawn(move || {
        let _held = registry.dispatch_returns.lock();
        panic!("poison the routes (test)");
    })
    .join();
    assert!(unit.registry().dispatch_returns.is_poisoned());
    assert!(
        !unit.registry().inbox().reports_torn(),
        "the state is whole"
    );
    let report = {
        let unit = Arc::clone(&unit);
        tokio::spawn(async move { unit.report("done").await })
    };
    let answer = report.await.expect("the reporting task does not panic");
    refused_with(answer, code::DECK_UNAVAILABLE);
    assert_eq!(
        unit.registry()
            .inbox()
            .read_reports(|r| r.claims.len())
            .unwrap(),
        0
    );
    assert_eq!(super::report_tests::store_files(&unit.deck.store).len(), 0);
    assert_eq!(unit.completed(), 0);
    assert!(
        unit.registry()
            .dispatched_units()
            .get(&unit.unit_id())
            .is_some_and(|u| u.completed_at_ms.is_none())
    );
    // Leaked on purpose: the registry's own teardown panics on a poisoned
    // lock, the documented boundary.
    std::mem::forget(Arc::try_unwrap(unit).ok());
}

/// Scenario: A terminal report A is paused after its save, and a duplicate B is admitted and waits. A is released to just after the unit mark: for 300 ms neither A nor B has an answer, the unit reads as reported, and the caller does not yet hold the report; a new duplicate C admitted now has no answer either. Released, A is accepted and B and C are answered `already_accepted` with A's report id; the caller holds exactly one report.
#[spec("orchestration/work-done/051")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_051_duplicates_are_answered_only_after_the_mark_and_the_commit() {
    let unit = Arc::new(unit("wd051", false).await);
    let (marked, release_marked) = unit
        .registry()
        .pause_at_for_test(crate::task_completion::PAUSE_MARKED);
    let (a, release_saved) =
        paused_report(&unit, crate::task_completion::PAUSE_SAVED, "the report").await;
    let spawn_duplicate = |unit: &Arc<Unit>| {
        let unit = Arc::clone(unit);
        tokio::spawn(async move { unit.report("the report").await })
    };
    let b = spawn_duplicate(&unit);
    tokio::time::sleep(Duration::from_millis(100)).await;
    release_saved.send(()).unwrap();
    marked.await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !a.is_finished() && !b.is_finished(),
        "no answer before the commit"
    );
    assert!(
        unit.registry()
            .dispatched_units()
            .get(&unit.unit_id())
            .is_some_and(|u| u.completed_at_ms.is_some()),
        "the unit reads as reported"
    );
    assert_eq!(unit.completed(), 0, "not yet published");
    let c = spawn_duplicate(&unit);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!c.is_finished());
    release_marked.send(()).unwrap();
    let a = accepted(a.await.unwrap());
    for duplicate in [b, c] {
        let answer = reply(duplicate.await.unwrap());
        assert_eq!(answer.disposition, WorkDoneDisposition::AlreadyAccepted);
        assert_eq!(answer.report_id, a.report_id);
    }
    assert_eq!(unit.completed(), 1);
}

/// Scenario: (b) as (a), with the unit's agent exiting on its own instead of being closed: `task_closed`, one `dispatch ended`. (g) a terminal report attests and is paused before the routes; its unit is closed and, with the tombstone knob at 1, evicted from the deck's memory; released, it is refused `unknown_task` with the forgotten wording — never `destination: none` — and nothing is saved, posted or ended a second time.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_b_g_an_exit_and_a_forgotten_unit() {
    // SAFETY: a nextest process runs one test.
    unsafe { std::env::set_var(crate::report_recovery::ENV_TOMBSTONES_MAX, "1") };
    // (b)
    let unit = Arc::new(unit("r027b", false).await);
    let (report, release) = paused_report(
        &unit,
        crate::task_completion::PAUSE_TERMINAL_RESOLVED,
        "done",
    )
    .await;
    kill_child(unit.registry(), unit.agent());
    eventually("the unit ends", || {
        unit_entry(&unit.deck, &unit.unit_id()).is_some()
    })
    .await;
    release.send(()).unwrap();
    refused_with(report.await.unwrap(), code::TASK_CLOSED);
    eventually("one dispatch ended", || unit.ended() == 1).await;

    // (g)
    let unit = Arc::new(self::unit("r027g", false).await);
    let (report, release) = paused_report(
        &unit,
        crate::task_completion::PAUSE_TERMINAL_RESOLVED,
        "done",
    )
    .await;
    unit.close();
    eventually("the unit ends", || {
        unit_entry(&unit.deck, &unit.unit_id()).is_some()
    })
    .await;
    let others: Vec<(String, String, String)> = (0..32)
        .map(|n| {
            (
                format!("r027g-p{n}"),
                format!("gone-{n}"),
                format!("u-r027g-{n}"),
            )
        })
        .collect();
    let refs: Vec<(&str, &str, &str)> = others
        .iter()
        .map(|(p, a, u)| (p.as_str(), a.as_str(), u.as_str()))
        .collect();
    unit.registry()
        .inbox()
        .insert_ended_units_for_test(unit.registry().as_ref(), &refs);
    assert!(
        unit_entry(&unit.deck, &unit.unit_id()).is_none(),
        "the unit's entry is evicted"
    );
    release.send(()).unwrap();
    let refusal = refused_with(report.await.unwrap(), code::UNKNOWN_TASK);
    assert!(refusal.message.unwrap().contains("no longer remembers"));
    assert_eq!(unit.ended(), 1, "no second dispatch ended");
    assert_eq!(unit.completed(), 0);
    assert_eq!(std::fs::read_dir(unit.deck.store.dir()).unwrap().count(), 0);
}
