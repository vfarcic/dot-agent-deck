//! PRD #1666 part 3, round 5 (plan D7, §6.2 `dispatch/return/026`): a
//! respawned unit terminal's replacement — `begin`, `publish`, `abort` — and
//! its successor's EOF, through the real registry, hub and store, with `cat`
//! stand-ins. The arms that need a terminal `work-done` arrive with the report
//! transaction.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use spec::spec;

use super::dispatch_lifecycle_tests::{
    Deck, deck, dispatch_orchestration, dispatch_single, eventually, inbox_of, kill_child, rows,
    spawn_cat, unit_entry,
};
use super::*;
use crate::report_recovery::{EndedHow, OutcomeCause, OutcomePhase, Publication, UnitEntry};
use crate::spawn::SpawnHandle;

fn indexed(deck: &Deck, pane: &str, agent: &str) -> Option<String> {
    deck.registry
        .inbox()
        .read_reports(|r| r.units.unit_of(pane, agent).map(str::to_string))
        .expect("report state")
}

fn pinned_unit(deck: &Deck, agent: &str) -> Option<String> {
    let inner = deck.registry.inner.lock_recovering();
    inner
        .agents
        .get(agent)
        .map(|a| a.attest_facts.unit.clone())
        .or_else(|| inner.pending_facts.get(agent).map(|f| f.unit.clone()))
        .flatten()
}

fn route_agent(deck: &Deck, pane: &str) -> Option<String> {
    deck.registry
        .dispatch_returns
        .lock_recovering()
        .resolve(pane)
        .map(|route| route.unit_agent_id.clone())
}

fn ended_notices(deck: &Deck, pane: &str) -> usize {
    inbox_of(deck, pane)
        .iter()
        .filter(|(subject, _)| subject == "dispatch ended")
        .count()
}

async fn dispatched(deck: &Deck, orchestration: bool, tag: &str) -> (String, String, SpawnHandle) {
    let caller_pane = format!("{tag}-caller");
    let caller_agent = spawn_cat(&deck.registry, &caller_pane);
    let unit = if orchestration {
        dispatch_orchestration(deck, tag, &caller_pane, &caller_agent).await
    } else {
        dispatch_single(deck, tag, &caller_pane, &caller_agent).await
    };
    (caller_pane, caller_agent, unit)
}

/// Scenario: A unit dispatched on generation A is respawned to B. The route moves to B, (P, B) is indexed to the unit and B's pinned unit is set; with a live caller, B's exit ends the unit once and posts one `dispatch ended` notice; with an orphaned caller it leaves one row instead. A single unit and an orchestration unit each.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_a_respawned_units_successor_ends_it_once() {
    for orchestration in [false, true] {
        for orphaned in [false, true] {
            let deck = deck();
            let tag = format!("r026a-{orchestration}-{orphaned}");
            let (caller_pane, caller_agent, unit) = dispatched(&deck, orchestration, &tag).await;
            let (pane, a) = (
                unit.delivery_pane_id.clone(),
                unit.delivery_agent_id.clone(),
            );
            let unit_id = unit.unit_id.clone().unwrap();
            let b = deck
                .registry
                .respawn_agent_for_pane(&pane, "cat")
                .await
                .expect("respawn the unit's terminal");
            assert_ne!(a, b);
            assert_eq!(
                route_agent(&deck, &pane).as_deref(),
                Some(b.as_str()),
                "{tag}"
            );
            assert_eq!(
                indexed(&deck, &pane, &b).as_deref(),
                Some(unit_id.as_str()),
                "{tag}"
            );
            assert_eq!(
                pinned_unit(&deck, &b).as_deref(),
                Some(unit_id.as_str()),
                "{tag}"
            );
            assert!(
                deck.registry.inbox().publication_on(&pane).is_none(),
                "{tag}"
            );
            if orphaned {
                kill_child(&deck.registry, &caller_agent);
                eventually("orphaned", || deck.registry.dispatch_edges().is_empty()).await;
            }
            kill_child(&deck.registry, &b);
            eventually("the unit ends", || unit_entry(&deck, &unit_id).is_some()).await;
            assert_eq!(
                unit_entry(&deck, &unit_id),
                Some(UnitEntry::Ended {
                    how: EndedHow::Exited
                }),
                "{tag}"
            );
            assert!(
                route_agent(&deck, &pane).is_none(),
                "{tag}: the route is gone"
            );
            if orphaned {
                let rows = rows(&deck);
                assert_eq!(rows.len(), 1, "{tag}");
                assert_eq!(rows[0].cause, OutcomeCause::RecipientExited);
                assert_eq!(ended_notices(&deck, &caller_pane), 0);
            } else {
                assert_eq!(ended_notices(&deck, &caller_pane), 1, "{tag}");
                assert!(rows(&deck).is_empty(), "{tag}");
            }
        }
    }
}

/// Scenario: A delayed predecessor EOF. A's exit cleanup is held until B holds the unit and its route, then released: no route is removed, the unit does not end, and no notice is posted.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_a_predecessors_late_eof_ends_nothing() {
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026d").await;
    let (pane, a) = (
        unit.delivery_pane_id.clone(),
        unit.delivery_agent_id.clone(),
    );
    let unit_id = unit.unit_id.clone().unwrap();
    let release = deck.registry.hold_exit_cleanup_for_test(&a);
    let b = deck
        .registry
        .respawn_agent_for_pane(&pane, "cat")
        .await
        .unwrap();
    assert_eq!(route_agent(&deck, &pane).as_deref(), Some(b.as_str()));
    release.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(route_agent(&deck, &pane).as_deref(), Some(b.as_str()));
    assert!(unit_entry(&deck, &unit_id).is_none());
    assert_eq!(ended_notices(&deck, &caller_pane), 0);
}

/// Scenario: B exits before the replacement is published: the respawn is paused (the synchronous pause) after B's spawn and before the leaf move; B is killed; B's EOF cleanup waits — the route still names A and nothing ended — until the pause is released; then the unit ends exactly once.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_a_successor_that_exits_before_publish_waits_for_it() {
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026e").await;
    let (pane, a) = (
        unit.delivery_pane_id.clone(),
        unit.delivery_agent_id.clone(),
    );
    let unit_id = unit.unit_id.clone().unwrap();
    let (reached, release) = deck
        .registry
        .pause_at_sync_for_test(PAUSE_RESPAWN_BEFORE_PUBLISH);
    let respawn = {
        let registry = Arc::clone(&deck.registry);
        let pane = pane.clone();
        tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
    };
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the respawn reaches its window");
    assert!(matches!(
        deck.registry.inbox().publication_on(&pane),
        Some(Publication::Replacement { .. })
    ));
    let b = deck
        .registry
        .pane_current_agent_id(&pane)
        .expect("B is published");
    kill_child(&deck.registry, &b);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(route_agent(&deck, &pane).as_deref(), Some(a.as_str()));
    assert!(unit_entry(&deck, &unit_id).is_none());
    release.send(()).unwrap();
    respawn.await.unwrap().unwrap();
    eventually("B's EOF ends the unit", || {
        unit_entry(&deck, &unit_id).is_some()
    })
    .await;
    assert_eq!(ended_notices(&deck, &caller_pane), 1);
    assert!(route_agent(&deck, &pane).is_none());
}

/// What a failed replacement left, for (f1)–(f3), (f2').
async fn assert_aborted(
    deck: &Deck,
    caller_pane: &str,
    pane: &str,
    unit_id: &str,
    b: Option<&str>,
) {
    eventually("the abort settles the unit", || {
        unit_entry(deck, unit_id).is_some()
    })
    .await;
    assert_eq!(
        unit_entry(deck, unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::RespawnFailed
        })
    );
    assert!(route_agent(deck, pane).is_none(), "the route is gone");
    assert!(deck.registry.inbox().publication_on(pane).is_none());
    assert!(
        deck.registry.dispatched_units().get(unit_id).is_none(),
        "the leaf unit ended"
    );
    eventually("one dispatch ended", || {
        ended_notices(deck, caller_pane) == 1
    })
    .await;
    match b {
        Some(b) => {
            assert_eq!(indexed(deck, pane, b).as_deref(), Some(unit_id));
            eventually("B is closed by its id", || {
                deck.registry.abort_closes_for_test() == vec![b.to_string()]
            })
            .await;
        }
        None => assert!(deck.registry.abort_closes_for_test().is_empty()),
    }
}

/// Scenario: (f1) the successor's spawn fails before its reservation: the unit ends `RespawnFailed`, its route goes, its leaf unit ends, its caller gets one `dispatch ended` notice, no publication is left, and nothing is closed (no successor was reserved).
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f1_a_failed_spawn_aborts_the_replacement() {
    for orchestration in [false, true] {
        let deck = deck();
        let (caller_pane, _, unit) =
            dispatched(&deck, orchestration, &format!("r026f1-{orchestration}")).await;
        let pane = unit.delivery_pane_id.clone();
        let unit_id = unit.unit_id.clone().unwrap();
        deck.registry
            .inject_fault_for_test(FAULT_SPAWN_FAILS, TestFault::Fail);
        let error = deck
            .registry
            .respawn_agent_for_pane(&pane, "cat")
            .await
            .expect_err("the spawn fails");
        assert!(error.to_string().contains("injected"), "{error}");
        assert_aborted(&deck, &caller_pane, &pane, &unit_id, None).await;
    }
}

/// Scenario: (f2) a panic after B's spawn and before the leaf move, (f3) one after the leaf move and before the publish section, and (f2') one inside the spawn after B's record is published, which hides B's id from the respawn. The respawn's window catches it and aborts in ordinary code: B is taken from the ticket's successor slot, indexed and pinned, the unit ends once, B is closed by its exact id, and the injected panic reaches the respawn's caller.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f2_f3_a_panic_in_the_window_aborts_with_the_reserved_successor() {
    for seam in [
        FAULT_RESPAWN_AFTER_SPAWN,
        FAULT_RESPAWN_BEFORE_PUBLISH_SECTION,
        FAULT_SPAWN_AFTER_INSERT,
    ] {
        let deck = deck();
        let (caller_pane, _, unit) = dispatched(&deck, false, &format!("r026f-{seam}")).await;
        let pane = unit.delivery_pane_id.clone();
        let unit_id = unit.unit_id.clone().unwrap();
        deck.registry
            .inject_fault_for_test(seam, TestFault::Panic("injected-window-panic"));
        let respawn = {
            let registry = Arc::clone(&deck.registry);
            let pane = pane.clone();
            tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
        };
        let panic = respawn.await.expect_err("the panic reaches the caller");
        let payload = panic.into_panic();
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"injected-window-panic"),
            "{seam}: the injected payload, resumed"
        );
        let b = deck
            .registry
            .inbox()
            .read_reports(|r| r.units.generations_of(&unit_id))
            .unwrap()
            .into_iter()
            .map(|(_, agent)| agent)
            .find(|agent| *agent != unit.delivery_agent_id)
            .expect("B indexed");
        assert_aborted(&deck, &caller_pane, &pane, &unit_id, Some(&b)).await;
    }
}

/// Scenario: (f5a) the abort is skipped and its latch dropped with no value, with no lock poisoned: B's reader runs the abort itself when B exits, so the unit ends `RespawnFailed`, one `dispatch ended` is posted and the publication goes.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f5a_a_dropped_latch_makes_the_reader_abort() {
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026f5a").await;
    let pane = unit.delivery_pane_id.clone();
    let unit_id = unit.unit_id.clone().unwrap();
    deck.registry
        .inject_fault_for_test(FAULT_RESPAWN_AFTER_SPAWN, TestFault::Panic("f5a"));
    deck.registry
        .inject_fault_for_test(FAULT_ABORT_SKIPPED, TestFault::Fail);
    let respawn = {
        let registry = Arc::clone(&deck.registry);
        let pane = pane.clone();
        tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
    };
    assert!(respawn.await.is_err());
    assert!(matches!(
        deck.registry.inbox().publication_on(&pane),
        Some(Publication::Replacement { .. })
    ));
    let b = deck
        .registry
        .pane_current_agent_id(&pane)
        .expect("B is live");
    kill_child(&deck.registry, &b);
    eventually("B's reader aborts", || {
        unit_entry(&deck, &unit_id).is_some()
    })
    .await;
    assert_eq!(
        unit_entry(&deck, &unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::RespawnFailed
        })
    );
    assert!(deck.registry.inbox().publication_on(&pane).is_none());
    eventually("one dispatch ended", || {
        ended_notices(&deck, &caller_pane) == 1
    })
    .await;
}

/// Scenario: (f6) a respawn cancelled while it awaits A's termination, before `begin`: no publication was installed, so nothing from the pane is held pending; the route and the unit are as before round 5 left them (the route still names A).
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f6_a_respawn_cancelled_before_begin_installs_nothing() {
    let deck = deck();
    let (_, _, unit) = dispatched(&deck, false, "r026f6").await;
    let (pane, a) = (
        unit.delivery_pane_id.clone(),
        unit.delivery_agent_id.clone(),
    );
    let (reached, _release) = deck.registry.pause_next_respawn_for_test();
    let respawn = {
        let registry = Arc::clone(&deck.registry);
        let pane = pane.clone();
        tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
    };
    reached.await.unwrap();
    respawn.abort();
    let _ = respawn.await;
    assert!(deck.registry.inbox().publication_on(&pane).is_none());
    assert_eq!(route_agent(&deck, &pane).as_deref(), Some(a.as_str()));
}

/// Scenario: (f7) a competing start during A's termination wait: the respawn is paused after it lifts A out; an ordinary start publishes C on the pane; released, `begin` installs the publication and B's reservation is refused because C is live, so the ticket's slot stays empty. The respawn returns that error, the unit ends once, the publication goes, and C is untouched: not indexed, not pinned, not closed.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f7_a_competing_start_is_never_taken_for_the_successor() {
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026f7").await;
    let pane = unit.delivery_pane_id.clone();
    let unit_id = unit.unit_id.clone().unwrap();
    let (reached, release) = deck.registry.pause_next_respawn_for_test();
    let respawn = {
        let registry = Arc::clone(&deck.registry);
        let pane = pane.clone();
        tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
    };
    reached.await.unwrap();
    let c = spawn_cat(&deck.registry, &pane);
    release.send(()).unwrap();
    let error = respawn
        .await
        .unwrap()
        .expect_err("B's reservation is refused");
    assert!(
        matches!(error, AgentPtyError::DuplicatePaneId(_)),
        "{error}"
    );
    eventually("the unit ends", || unit_entry(&deck, &unit_id).is_some()).await;
    assert_eq!(
        unit_entry(&deck, &unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::RespawnFailed
        })
    );
    assert!(deck.registry.inbox().publication_on(&pane).is_none());
    assert_eq!(ended_notices(&deck, &caller_pane), 1);
    assert!(indexed(&deck, &pane, &c).is_none(), "C is not indexed");
    assert!(pinned_unit(&deck, &c).is_none(), "C is not pinned");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        deck.registry.abort_closes_for_test().is_empty(),
        "nothing closed"
    );
    assert_eq!(
        deck.registry.pane_current_agent_id(&pane).as_deref(),
        Some(c.as_str())
    );
}

/// Scenario: (f2s) an exited record C0 is left on the pane beside the replacement — started and ended there while the respawn was paused after lifting A out — and the window then panics, once after B's spawn and once inside the spawn after B is published. The abort indexes, pins and closes B by its reserved id; C0 is not indexed, pinned or closed.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f2s_a_stale_exited_record_is_never_taken_for_the_successor() {
    for seam in [FAULT_RESPAWN_AFTER_SPAWN, FAULT_SPAWN_AFTER_INSERT] {
        let deck = deck();
        let (caller_pane, _, unit) = dispatched(&deck, false, &format!("r026f2s-{seam}")).await;
        let pane = unit.delivery_pane_id.clone();
        let unit_id = unit.unit_id.clone().unwrap();
        let (reached, release) = deck.registry.pause_next_respawn_for_test();
        let respawn = {
            let registry = Arc::clone(&deck.registry);
            let pane = pane.clone();
            tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
        };
        reached.await.unwrap();
        let c0 = spawn_cat(&deck.registry, &pane);
        kill_child(&deck.registry, &c0);
        eventually("C0 exited", || {
            deck.registry
                .agent_record_any(&c0)
                .is_some_and(|r| r.crashed == Some(true))
        })
        .await;
        deck.registry
            .inject_fault_for_test(seam, TestFault::Panic("f2s"));
        release.send(()).unwrap();
        assert!(respawn.await.is_err(), "{seam}");
        let b = deck
            .registry
            .inbox()
            .read_reports(|r| r.units.generations_of(&unit_id))
            .unwrap()
            .into_iter()
            .map(|(_, agent)| agent)
            .find(|agent| *agent != unit.delivery_agent_id)
            .expect("B indexed");
        assert_ne!(b, c0);
        assert_aborted(&deck, &caller_pane, &pane, &unit_id, Some(&b)).await;
        assert!(indexed(&deck, &pane, &c0).is_none(), "{seam}");
        assert!(pinned_unit(&deck, &c0).is_none(), "{seam}");
        assert!(
            deck.registry.agent_record_any(&c0).is_some(),
            "{seam}: C0's record kept"
        );
    }
}

/// Scenario: (g) close, publish and eviction together. The respawn is paused in its window; a close of the pane settles the unit (`Ended { Closed }`, one `dispatch ended`) and keeps the publication, marked closed. Released, `publish` indexes B and removes the publication, resurrects no route, and posts no second notice.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_g_a_close_during_the_replacement_keeps_the_publication_until_publish() {
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026g").await;
    let pane = unit.delivery_pane_id.clone();
    let unit_id = unit.unit_id.clone().unwrap();
    let (reached, release) = deck
        .registry
        .pause_at_sync_for_test(PAUSE_RESPAWN_BEFORE_PUBLISH);
    let respawn = {
        let registry = Arc::clone(&deck.registry);
        let pane = pane.clone();
        tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
    };
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    drop(deck.registry.begin_pane_close(&pane));
    assert_eq!(
        unit_entry(&deck, &unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::Closed
        })
    );
    assert!(matches!(
        deck.registry.inbox().publication_on(&pane),
        Some(Publication::Replacement { closed: true, .. })
    ));
    assert_eq!(ended_notices(&deck, &caller_pane), 1);
    release.send(()).unwrap();
    let b = respawn.await.unwrap().unwrap();
    assert!(deck.registry.inbox().publication_on(&pane).is_none());
    assert_eq!(indexed(&deck, &pane, &b).as_deref(), Some(unit_id.as_str()));
    assert!(
        route_agent(&deck, &pane).is_none(),
        "no route is resurrected"
    );
    assert_eq!(
        ended_notices(&deck, &caller_pane),
        1,
        "no second dispatch ended"
    );
    assert_eq!(
        unit_entry(&deck, &unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::Closed
        })
    );
    let _ = OutcomePhase::DispatchEnded;
}

/// The child-process case variable for the arms below.
const CHILD: &str = "DOT_AGENT_DECK_TEST_REPLACEMENT_CHILD";

/// Re-run `test` in a child process, and require that it passed rather than
/// aborted: a panic during an unwind would read as `SIGABRT`.
fn in_child(test: &str) {
    let name = format!("agent_pty::replacement_tests::{test}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("re-execute the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the child did not survive ({:?}):\n{stdout}\n{stderr}",
        output.status
    );
}

fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

async fn respawn_panicking(deck: &Deck, pane: &str) -> Box<dyn std::any::Any + Send> {
    let registry = Arc::clone(&deck.registry);
    let pane = pane.to_string();
    tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
        .await
        .expect_err("the respawn panics")
        .into_panic()
}

/// Scenario: (f4) a panic inside the replacement's publish section, between the compare-and-set and the index, run in a child process. It poisons the routes and tears the report state; the abort finds them unavailable and writes nothing: the process survives, the poison is not cleared, the state reads torn, the route already moved to B by the compare-and-set is left so, the leaf unit stays on B, no `dispatch ended` is posted, B is not closed, the injected payload reaches the respawn's caller, and the rest of the inbox still delivers and acknowledges a message.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f4_a_panic_inside_publish_leaves_a_torn_state_and_a_live_daemon() {
    if !is_child() {
        return in_child(
            "return_026_f4_a_panic_inside_publish_leaves_a_torn_state_and_a_live_daemon",
        );
    }
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026f4").await;
    let pane = unit.delivery_pane_id.clone();
    let unit_id = unit.unit_id.clone().unwrap();
    crate::inbox_hub::report_seams::on_next_publish(|| {
        std::panic::panic_any("injected-publish-panic")
    });
    let payload = respawn_panicking(&deck, &pane).await;
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"injected-publish-panic")
    );
    assert!(
        deck.registry.dispatch_returns.is_poisoned(),
        "R2's poison is not cleared"
    );
    assert!(deck.registry.inbox().reports_torn());
    let b = deck
        .registry
        .pane_current_agent_id(&pane)
        .expect("B is live");
    assert_eq!(
        route_agent(&deck, &pane).as_deref(),
        Some(b.as_str()),
        "the CAS ran"
    );
    assert!(pinned_unit(&deck, &b).is_none(), "the abort pinned nothing");
    assert_eq!(
        deck.registry
            .dispatched_units()
            .get(&unit_id)
            .map(|u| u.kind.terminal().1.to_string()),
        Some(b.clone()),
        "the leaf unit stays on B"
    );
    assert_eq!(ended_notices(&deck, &caller_pane), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(deck.registry.abort_closes_for_test().is_empty());
    assert!(deck.registry.child_pid(&b).is_some(), "B is running");
    // The rest of the hub carries on.
    let other = spawn_cat(&deck.registry, "r026f4-other");
    let id = deck
        .registry
        .inbox()
        .post_direct(
            crate::inbox::Recipient::bound("r026f4-other", &other),
            crate::inbox::Content::from_deck("notice", "still works", "body"),
        )
        .expect("an unrelated post");
    assert_eq!(
        deck.registry
            .inbox()
            .ack_in_store_for_test(id.as_str(), "r026f4-other", &other),
        crate::inbox::AckOutcome::Acknowledged
    );
    // B's EOF: its reader reads `Unavailable`; its route sweep panics on the
    // poisoned routes in its own thread, and the process survives.
    kill_child(&deck.registry, &b);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(ended_notices(&deck, &caller_pane), 0);
    // Leaked on purpose: the registry's own teardown panics on poisoned
    // locks, the documented boundary.
    std::mem::forget(deck);
}

/// Scenario: (f4') the report state is torn by another thread with the routes healthy, after a panic in the window and before the abort's section, in a child process. The abort takes the routes, finds the state torn and writes nothing: the route still names A, the leaf unit is still on A, B carries no pinned unit, the routes are not poisoned, no `dispatch ended` is posted, B is running, and the injected payload reaches the caller.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f4p_a_torn_state_with_healthy_routes_makes_the_abort_write_nothing() {
    if !is_child() {
        return in_child(
            "return_026_f4p_a_torn_state_with_healthy_routes_makes_the_abort_write_nothing",
        );
    }
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026f4p").await;
    let (pane, a) = (
        unit.delivery_pane_id.clone(),
        unit.delivery_agent_id.clone(),
    );
    let unit_id = unit.unit_id.clone().unwrap();
    deck.registry
        .inject_fault_for_test(FAULT_RESPAWN_AFTER_SPAWN, TestFault::Panic("injected-f4p"));
    deck.registry
        .inject_fault_for_test(FAULT_TEAR_BEFORE_ABORT, TestFault::Fail);
    let payload = respawn_panicking(&deck, &pane).await;
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"injected-f4p"));
    assert!(deck.registry.inbox().reports_torn());
    assert!(!deck.registry.dispatch_returns.is_poisoned());
    assert_eq!(route_agent(&deck, &pane).as_deref(), Some(a.as_str()));
    assert_eq!(
        deck.registry
            .dispatched_units()
            .get(&unit_id)
            .map(|u| u.kind.terminal().1.to_string()),
        Some(a.clone())
    );
    let b = deck
        .registry
        .pane_current_agent_id(&pane)
        .expect("B is live");
    assert!(pinned_unit(&deck, &b).is_none());
    assert_eq!(ended_notices(&deck, &caller_pane), 0);
    assert!(deck.registry.child_pid(&b).is_some());
    std::mem::forget(deck);
}

/// Scenario: (f4'') an unrelated hub panic is not a torn report state: the hub's lock is poisoned by a panic in an existing hub path before a window panic, in a child process. The hub keeps recovering, the report state reads whole, and the abort completes exactly as (f2)'s does.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f4pp_an_unrelated_hub_panic_leaves_the_abort_working() {
    if !is_child() {
        return in_child("return_026_f4pp_an_unrelated_hub_panic_leaves_the_abort_working");
    }
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026f4pp").await;
    let pane = unit.delivery_pane_id.clone();
    let unit_id = unit.unit_id.clone().unwrap();
    deck.registry.inbox().poison_for_test();
    assert!(!deck.registry.inbox().reports_torn());
    deck.registry
        .inject_fault_for_test(FAULT_RESPAWN_AFTER_SPAWN, TestFault::Panic("injected-f4pp"));
    let payload = respawn_panicking(&deck, &pane).await;
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"injected-f4pp"));
    let b = deck
        .registry
        .inbox()
        .read_reports(|r| r.units.generations_of(&unit_id))
        .unwrap()
        .into_iter()
        .map(|(_, agent)| agent)
        .find(|agent| *agent != unit.delivery_agent_id)
        .expect("B indexed");
    assert_aborted(&deck, &caller_pane, &pane, &unit_id, Some(&b)).await;
}

/// Scenario: (f5b) the abort's own section panics, in a child process, so its latch is dropped with no value: B's reader runs the abort itself, finds the report state torn and the routes poisoned, and does nothing; the process survives. A second arm panics after the section and before the latch: the section's writes stand, and B's reader finds the publication gone and does nothing more.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f5b_a_panicking_abort_leaves_the_reader_to_find_what_it_left() {
    if !is_child() {
        return in_child("return_026_f5b_a_panicking_abort_leaves_the_reader_to_find_what_it_left");
    }
    for after_section in [false, true] {
        let deck = deck();
        let (caller_pane, _, unit) =
            dispatched(&deck, false, &format!("r026f5b-{after_section}")).await;
        let pane = unit.delivery_pane_id.clone();
        let unit_id = unit.unit_id.clone().unwrap();
        deck.registry
            .inject_fault_for_test(FAULT_RESPAWN_AFTER_SPAWN, TestFault::Panic("injected-f5b"));
        if after_section {
            deck.registry.inject_fault_for_test(
                FAULT_ABORT_AFTER_SECTION,
                TestFault::Panic("abort-after-section"),
            );
        } else {
            crate::inbox_hub::report_seams::on_next_abort(|| {
                std::panic::panic_any("abort-section")
            });
        }
        let _ = respawn_panicking(&deck, &pane).await;
        let b = deck
            .registry
            .pane_current_agent_id(&pane)
            .expect("B is live");
        kill_child(&deck.registry, &b);
        tokio::time::sleep(Duration::from_millis(500)).await;
        if after_section {
            assert_eq!(
                unit_entry(&deck, &unit_id),
                Some(UnitEntry::Ended {
                    how: EndedHow::RespawnFailed
                })
            );
            assert!(deck.registry.inbox().publication_on(&pane).is_none());
            assert_eq!(ended_notices(&deck, &caller_pane), 1);
        } else {
            assert!(deck.registry.inbox().reports_torn());
            assert!(deck.registry.dispatch_returns.is_poisoned());
            assert_eq!(ended_notices(&deck, &caller_pane), 0);
        }
        std::mem::forget(deck);
    }
}

/// Scenario: (f8) B exits and the pane is reused before the abort: the window is paused after B is published and before the leaf move; B is killed, and its reader waits on its latch with B's record still there, exited; an ordinary start publishes D on the pane; released with a panic before the publish section, the abort indexes and pins B and closes B by its id, while D is not indexed, pinned or closed.
#[spec("dispatch/return/026")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_026_f8_a_reused_pane_is_never_taken_for_the_successor() {
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "r026f8").await;
    let pane = unit.delivery_pane_id.clone();
    let unit_id = unit.unit_id.clone().unwrap();
    let (reached, release) = deck
        .registry
        .pause_at_sync_for_test(PAUSE_RESPAWN_BEFORE_PUBLISH);
    let respawn = {
        let registry = Arc::clone(&deck.registry);
        let pane = pane.clone();
        tokio::spawn(async move { registry.respawn_agent_for_pane(&pane, "cat").await })
    };
    tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    let b = deck
        .registry
        .pane_current_agent_id(&pane)
        .expect("B is published");
    kill_child(&deck.registry, &b);
    eventually("B exited", || {
        deck.registry
            .agent_record_any(&b)
            .is_some_and(|r| r.crashed == Some(true))
    })
    .await;
    let d = spawn_cat(&deck.registry, &pane);
    deck.registry.inject_fault_for_test(
        FAULT_RESPAWN_BEFORE_PUBLISH_SECTION,
        TestFault::Panic("injected-f8"),
    );
    release.send(()).unwrap();
    assert!(respawn.await.is_err());
    assert_eq!(indexed(&deck, &pane, &b).as_deref(), Some(unit_id.as_str()));
    eventually("B is closed", || {
        deck.registry.abort_closes_for_test() == vec![b.clone()]
    })
    .await;
    assert!(indexed(&deck, &pane, &d).is_none());
    assert!(pinned_unit(&deck, &d).is_none());
    assert_eq!(
        deck.registry.pane_current_agent_id(&pane).as_deref(),
        Some(d.as_str())
    );
    assert_eq!(ended_notices(&deck, &caller_pane), 1);
    assert_eq!(
        unit_entry(&deck, &unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::RespawnFailed
        })
    );
}

/// Scenario: A dispatch whose reply room is armed across its handling, and whose initial publication is installed, unwinds from a panic inside its unit registration's section, in a child process. The registration's tracker and routes are poisoned and the report state torn; the publication guard skips the poisoned routes without logging, counted once; the reply room comes back through the recovering hub; the injected payload escapes as an ordinary task panic; the inbox still delivers; and the next dispatch is refused before anything starts as the deck being degraded.
#[spec("orchestration/work-done/056")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_056_a_dispatch_unwinding_through_registration_leaves_a_live_daemon() {
    if !is_child() {
        return in_child(
            "work_done_056_a_dispatch_unwinding_through_registration_leaves_a_live_daemon",
        );
    }
    use std::sync::atomic::Ordering;
    let deck = deck();
    let caller_agent = spawn_cat(&deck.registry, "w056-caller");
    let rooms_before = deck.registry.inbox().reserved_replies();
    crate::inbox_hub::report_seams::on_next_registration(|| {
        std::panic::panic_any("injected-registration-panic")
    });
    let registry = Arc::clone(&deck.registry);
    let dir = deck.root.path().join("w056");
    let origin = crate::dispatched_units::UnitOrigin {
        name: "w056".to_string(),
        worktree: dir.clone(),
        branch: "dispatch/w056".to_string(),
        clone_dir: deck.root.path().to_path_buf(),
        dispatcher: crate::dispatched_units::Dispatcher {
            pane_id: "w056-caller".to_string(),
            agent_id: caller_agent.clone(),
        },
    };
    let caller = super::dispatch_lifecycle_tests::caller("w056-caller", &caller_agent, "w056");
    let task = tokio::spawn(async move {
        let _room = registry.inbox().reserve_reply().expect("room");
        let req = crate::spawn::SpawnRequest {
            task_name: "dispatch-w056".to_string(),
            working_dir: dir.to_string_lossy().into_owned(),
            command: None,
            prompt: "task".to_string(),
            resolved_target: Some(crate::spawn::SpawnTarget::SingleAgent {
                command: Some("cat".to_string()),
                inherited: None,
            }),
            compose_orchestrator_context: None,
            seed_reason: None,
        };
        let _ = crate::spawn::spawn_dispatched_unit(
            req,
            &registry,
            &super::dispatch_lifecycle_tests::Silent,
            None,
            true,
            None,
            origin,
            caller,
        )
        .await;
    });
    let payload = task.await.expect_err("the dispatch panics").into_panic();
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"injected-registration-panic")
    );
    assert!(deck.registry.delegations.is_poisoned());
    assert!(deck.registry.dispatch_returns.is_poisoned());
    assert!(deck.registry.inbox().reports_torn());
    assert_eq!(
        crate::lock_rank::SKIPPED_IN_UNWIND_TOTAL.load(Ordering::Relaxed),
        1,
        "the publication guard skipped the poisoned routes once, during the unwind"
    );
    assert_eq!(
        deck.registry.inbox().reserved_replies(),
        rooms_before,
        "the armed reply room came back through the recovering hub"
    );
    let other = spawn_cat(&deck.registry, "w056-other");
    let id = deck
        .registry
        .inbox()
        .post_direct(
            crate::inbox::Recipient::bound("w056-other", &other),
            crate::inbox::Content::from_deck("notice", "still works", "body"),
        )
        .unwrap();
    assert_eq!(
        deck.registry
            .inbox()
            .ack_in_store_for_test(id.as_str(), "w056-other", &other),
        crate::inbox::AckOutcome::Acknowledged
    );
    let (code, message) = crate::dispatch_return::admit_dispatch_result(&deck.registry)
        .err()
        .expect("the next dispatch is refused");
    assert_eq!(code, "deck_unavailable");
    assert!(message.contains("internal error") && message.contains("Restart the deck daemon"));
    std::mem::forget(deck);
}

/// Scenario: (c) a respawn's successor spawn panics after its fork and before its record is published, inside the respawn's window, with a replacement ticket present and the hub's lock poisoned, in a child process. The spawn reservation gives itself up during the unwind (its pending entry and token gone), the respawn's window catches the panic and aborts the replacement, which ends the unit and closes no other agent, and the injected payload reaches the respawn's caller.
#[spec("orchestration/work-done/057")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_done_057_c_a_spawn_unwinding_inside_the_replacement_window() {
    if !is_child() {
        return in_child("work_done_057_c_a_spawn_unwinding_inside_the_replacement_window");
    }
    let deck = deck();
    let (caller_pane, _, unit) = dispatched(&deck, false, "w057c").await;
    let pane = unit.delivery_pane_id.clone();
    let unit_id = unit.unit_id.clone().unwrap();
    deck.registry.inbox().poison_for_test();
    deck.registry.inject_fault_for_test(
        FAULT_SPAWN_BEFORE_PUBLISH,
        TestFault::Panic("injected-057c"),
    );
    let payload = respawn_panicking(&deck, &pane).await;
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"injected-057c"));
    {
        let inner = deck.registry.inner.lock();
        assert!(
            inner.pending_spawns.is_empty(),
            "the reservation gave itself up"
        );
        assert!(inner.pending_hook_tokens.is_empty());
    }
    eventually("the abort ends the unit", || {
        unit_entry(&deck, &unit_id).is_some()
    })
    .await;
    assert_eq!(
        unit_entry(&deck, &unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::RespawnFailed
        })
    );
    eventually("one dispatch ended", || {
        ended_notices(&deck, &caller_pane) == 1
    })
    .await;
    assert!(deck.registry.pane_current_agent_id(&pane).is_none());
}
