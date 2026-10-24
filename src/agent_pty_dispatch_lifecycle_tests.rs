//! PRD #1666 part 3, round 5 (plan §6.2): a dispatched unit's lifecycle —
//! routes orphaned and ended, `dispatch ended`, dispatch results retained,
//! fallback slots — through the real registry, hub and retained store, with
//! `cat` stand-ins for every agent. A child module of `agent_pty`, so it can
//! read the routes and records directly.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use spec::spec;

use super::*;
use crate::report_recovery::{
    DeckOutcome, EndedHow, OutcomeCause, OutcomePhase, OutcomeStorage, ReportProvenance, UnitEntry,
};
use crate::retained_store::{Now, RetainedStore, StoreConfig};
use crate::spawn::{SpawnHandle, SpawnRequest, SpawnTarget};

/// A notifier that discards everything.
pub(super) struct Silent;

impl crate::scheduler::Notifier for Silent {
    fn notify(&self, _: crate::scheduler::NotifyEvent) {}
}

/// A registry with a retained store and the daemon's live check installed.
pub(super) struct Deck {
    pub(super) registry: Arc<AgentPtyRegistry>,
    pub(super) store: Arc<RetainedStore>,
    pub(super) root: tempfile::TempDir,
}

impl Drop for Deck {
    fn drop(&mut self) {
        self.registry.shutdown_all();
    }
}

pub(super) fn deck_with(config: StoreConfig) -> Deck {
    let root = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let store = RetainedStore::open(&state, config, Now::current());
    registry.inbox().set_retained_store(Arc::clone(&store));
    let live = Arc::downgrade(&registry);
    registry
        .inbox()
        .set_live_agent_check(Arc::new(move |pane: &str, agent: &str| {
            live.upgrade()
                .is_some_and(|r| r.pane_current_agent_id(pane).as_deref() == Some(agent))
        }));
    Deck {
        registry,
        store,
        root,
    }
}

pub(super) fn deck() -> Deck {
    deck_with(StoreConfig::default())
}

/// A `cat` agent on `pane`.
pub(super) fn spawn_cat(registry: &Arc<AgentPtyRegistry>, pane: &str) -> String {
    registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn a cat")
}

/// End `agent`'s child on its own, as a crash would: its record stays.
pub(super) fn kill_child(registry: &AgentPtyRegistry, agent: &str) {
    let pid = registry.child_pid(agent).expect("a live child");
    // SAFETY: a pid this registry spawned; SIGKILL has no handler to run.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

pub(super) async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(super) fn caller(
    pane: &str,
    agent: &str,
    unit: &str,
) -> crate::dispatch_return::DispatchCaller {
    crate::dispatch_return::DispatchCaller {
        pane_id: pane.to_string(),
        agent_id: agent.to_string(),
        unit_name: unit.to_string(),
    }
}

fn origin(
    deck: &Deck,
    name: &str,
    caller_pane: &str,
    caller_agent: &str,
) -> crate::dispatched_units::UnitOrigin {
    crate::dispatched_units::UnitOrigin {
        name: name.to_string(),
        worktree: deck.root.path().join(name),
        branch: format!("dispatch/{name}"),
        clone_dir: deck.root.path().to_path_buf(),
        dispatcher: crate::dispatched_units::Dispatcher {
            pane_id: caller_pane.to_string(),
            agent_id: caller_agent.to_string(),
        },
    }
}

/// Dispatch a single `cat` unit for `caller`.
pub(super) async fn dispatch_single(
    deck: &Deck,
    name: &str,
    caller_pane: &str,
    caller_agent: &str,
) -> SpawnHandle {
    let dir = deck.root.path().join(name);
    let req = SpawnRequest {
        task_name: format!("dispatch-{name}"),
        working_dir: dir.to_string_lossy().into_owned(),
        command: None,
        prompt: format!("task for {name}"),
        resolved_target: Some(SpawnTarget::SingleAgent {
            command: Some("cat".to_string()),
            inherited: None,
        }),
        compose_orchestrator_context: None,
        seed_reason: None,
    };
    crate::spawn::spawn_dispatched_unit(
        req,
        &deck.registry,
        &Silent,
        None,
        true,
        None,
        origin(deck, name, caller_pane, caller_agent),
        caller(caller_pane, caller_agent, name),
    )
    .await
    .expect("dispatch a single unit")
}

/// Dispatch a two-role `cat` orchestration for `caller`.
pub(super) async fn dispatch_orchestration(
    deck: &Deck,
    name: &str,
    caller_pane: &str,
    caller_agent: &str,
) -> SpawnHandle {
    use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
    let dir = deck.root.path().join(name);
    let role = |idx: usize, role: &str| crate::spawn::RoleSpawn {
        agent_type: None,
        role_index: idx,
        role_name: role.to_string(),
        command: "cat".to_string(),
        is_start_role: idx == 0,
    };
    let req = SpawnRequest {
        task_name: format!("dispatch-{name}"),
        working_dir: dir.to_string_lossy().into_owned(),
        command: None,
        prompt: format!("task for {name}"),
        resolved_target: Some(SpawnTarget::Orchestration {
            name: name.to_string(),
            roles: vec![role(0, "orchestrator"), role(1, "worker")],
            config: Box::new(OrchestrationConfig {
                default: false,
                name: name.to_string(),
                roles: vec![OrchestrationRoleConfig {
                    agent: None,
                    name: "orchestrator".to_string(),
                    command: "cat".to_string(),
                    start: true,
                    description: None,
                    prompt_template: None,
                    clear: false,
                }],
            }),
        }),
        compose_orchestrator_context: None,
        seed_reason: None,
    };
    crate::spawn::spawn_dispatched_unit(
        req,
        &deck.registry,
        &Silent,
        None,
        true,
        None,
        origin(deck, name, caller_pane, caller_agent),
        caller(caller_pane, caller_agent, name),
    )
    .await
    .expect("dispatch an orchestration")
}

pub(super) fn unit_entry(deck: &Deck, unit: &str) -> Option<UnitEntry> {
    deck.registry
        .inbox()
        .read_reports(|r| r.units.get(unit).cloned())
        .expect("report state")
}

pub(super) fn rows(deck: &Deck) -> Vec<DeckOutcome> {
    deck.registry
        .inbox()
        .deck_outcomes()
        .expect("report state")
        .outcomes
}

/// The subjects of `pane`'s messages, with their provenance.
pub(super) fn inbox_of(deck: &Deck, pane: &str) -> Vec<(String, Option<ReportProvenance>)> {
    deck.registry
        .inbox()
        .list(pane)
        .into_iter()
        .map(|m| (m.subject, m.provenance))
        .collect()
}

fn ended_notices(deck: &Deck, pane: &str) -> usize {
    inbox_of(deck, pane)
        .iter()
        .filter(|(subject, _)| subject == "dispatch ended")
        .count()
}

/// Scenario: A unit is dispatched by a caller, and the caller then goes away three ways — its pane closed, its process exiting on its own, and a respawn replacing it. Each time the unit's route is kept but orphaned, leaves the dispatch tree, and the caller's successor gets no edge.
#[spec("dispatch/return/009")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_009_a_gone_caller_orphans_its_units_routes() {
    for how in ["close", "eof", "respawn"] {
        let deck = deck();
        let caller_pane = format!("ret009-caller-{how}");
        let caller_agent = spawn_cat(&deck.registry, &caller_pane);
        let unit =
            dispatch_single(&deck, &format!("ret009-{how}"), &caller_pane, &caller_agent).await;
        assert_eq!(deck.registry.dispatch_edges().len(), 1, "{how}");
        match how {
            "close" => {
                drop(deck.registry.begin_pane_close(&caller_pane));
            }
            "eof" => {
                kill_child(&deck.registry, &caller_agent);
                eventually("the caller's EOF orphans its route", || {
                    deck.registry.dispatch_edges().is_empty()
                })
                .await;
            }
            _ => {
                let successor = deck
                    .registry
                    .respawn_agent_for_pane(&caller_pane, "cat")
                    .await
                    .expect("respawn the caller");
                assert_ne!(successor, caller_agent);
                eventually("the predecessor's EOF orphans its route", || {
                    deck.registry.dispatch_edges().is_empty()
                })
                .await;
            }
        }
        let returns = deck.registry.dispatch_returns.lock();
        let route = returns
            .resolve(&unit.delivery_pane_id)
            .unwrap_or_else(|| panic!("{how}: the route is kept"));
        assert!(route.orphaned, "{how}");
        drop(returns);
        assert!(
            deck.registry.dispatch_edges().is_empty(),
            "{how}: no edge for anyone"
        );
        assert!(
            inbox_of(&deck, &caller_pane)
                .iter()
                .all(|(s, _)| s != "dispatch ended"),
            "{how}: nothing goes to whoever holds the caller's pane"
        );
    }
}

/// Scenario: Two dispatch results are retained and posted to a caller that exited during the spawn. Each becomes a `dispatch_started` row naming its own report id, whose saved file reads the start text in its frame; the successor's inbox gets nothing.
#[spec("dispatch/return/010")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_010_a_start_result_for_a_gone_caller_is_a_row_with_its_file() {
    let deck = deck();
    for n in 0..2 {
        let room = crate::dispatch_return::admit_dispatch_result(&deck.registry)
            .expect("admitted")
            .expect("a store is installed");
        let text = format!("dispatch: spawned isolated agent for 'ret010-{n}' START-SENTINEL-{n}");
        let record = crate::dispatch_return::retain_dispatch_result(
            &deck.registry,
            Some(room),
            "ret010-caller",
            "gone-agent",
            &format!("ret010-{n}"),
            &text,
            true,
            Some(format!("unit-{n}")),
        )
        .await
        .expect("retained");
        assert_eq!(record.storage, OutcomeStorage::Artifact);
        let reply = deck.registry.inbox().reserve_reply().unwrap();
        crate::dispatch_return::post_dispatch_result(
            &deck.registry,
            reply,
            "ret010-caller",
            "gone-agent",
            &format!("ret010-{n}"),
            true,
            &text,
            Some(record.clone()),
        );
        let file = std::fs::read_to_string(
            deck.store
                .dir()
                .join(crate::retained_store::own_name(&record.report_id)),
        )
        .unwrap();
        assert!(file.contains(&format!("START-SENTINEL-{n}")), "{file}");
    }
    let rows = rows(&deck);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.phase == OutcomePhase::DispatchStarted
        && r.cause == OutcomeCause::RecipientExited
        && r.storage == OutcomeStorage::Artifact));
    assert_ne!(rows[0].report_id, rows[1].report_id);
    assert!(deck.registry.inbox().list("ret010-caller").is_empty());
}

/// Scenario: A dispatched unit exits while its live caller's inbox is full. The `dispatch ended` notice the caller is owed cannot go in, so it becomes a recovery row with cause `recipient_refused`; the unit's entry reads ended.
#[spec("dispatch/return/011")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_011_a_refused_dispatch_ended_notice_is_a_recipient_refused_row() {
    let deck = deck();
    let caller_agent = spawn_cat(&deck.registry, "ret011-caller");
    let unit = dispatch_single(&deck, "ret011", "ret011-caller", &caller_agent).await;
    let unit_id = unit.unit_id.clone().unwrap();
    // Fill the caller's inbox.
    let to = crate::inbox::Recipient::bound("ret011-caller", &caller_agent);
    while deck
        .registry
        .inbox()
        .post_direct(
            to.clone(),
            crate::inbox::Content::from_deck("notice", "filler", "x"),
        )
        .is_ok()
    {}
    kill_child(&deck.registry, &unit.delivery_agent_id);
    eventually("the unit's end is recorded", || {
        unit_entry(&deck, &unit_id).is_some()
    })
    .await;
    assert_eq!(
        unit_entry(&deck, &unit_id),
        Some(UnitEntry::Ended {
            how: EndedHow::Exited
        })
    );
    let rows = rows(&deck);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].phase, OutcomePhase::DispatchEnded);
    assert_eq!(rows[0].cause, OutcomeCause::RecipientRefused);
    assert_eq!(rows[0].storage, OutcomeStorage::None);
    assert_eq!(ended_notices(&deck, "ret011-caller"), 0);
}

/// Scenario: A unit's start is acknowledged; then (orphaned arm) its caller exits and the unit exits without a report, which leaves one `dispatch_ended` recovery row with cause `recipient_exited`; and (live arm) the unit exits while its caller is still there, which posts one `dispatch ended` notice to the caller's inbox and makes no row. A single unit and an orchestration unit each.
#[spec("dispatch/return/017")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_017_a_unit_that_ends_without_a_report_tells_its_caller_once() {
    for orchestration in [false, true] {
        for orphaned in [true, false] {
            let deck = deck();
            let tag = format!("ret017-{orchestration}-{orphaned}");
            let caller_pane = format!("{tag}-caller");
            let caller_agent = spawn_cat(&deck.registry, &caller_pane);
            let unit = if orchestration {
                dispatch_orchestration(&deck, &tag, &caller_pane, &caller_agent).await
            } else {
                dispatch_single(&deck, &tag, &caller_pane, &caller_agent).await
            };
            let unit_id = unit.unit_id.clone().unwrap();
            if orphaned {
                kill_child(&deck.registry, &caller_agent);
                eventually("the caller is orphaned", || {
                    deck.registry.dispatch_edges().is_empty()
                })
                .await;
            }
            kill_child(&deck.registry, &unit.delivery_agent_id);
            eventually("the unit's end is recorded", || {
                unit_entry(&deck, &unit_id).is_some()
            })
            .await;
            let rows = rows(&deck);
            if orphaned {
                assert_eq!(rows.len(), 1, "{tag}");
                assert_eq!(rows[0].phase, OutcomePhase::DispatchEnded);
                assert_eq!(rows[0].cause, OutcomeCause::RecipientExited);
                assert!(rows[0].summary.contains("it exited"), "{}", rows[0].summary);
                assert_eq!(ended_notices(&deck, &caller_pane), 0, "{tag}");
            } else {
                assert!(rows.is_empty(), "{tag}: {rows:?}");
                assert_eq!(ended_notices(&deck, &caller_pane), 1, "{tag}");
                let (_, provenance) = inbox_of(&deck, &caller_pane)
                    .into_iter()
                    .find(|(s, _)| s == "dispatch ended")
                    .unwrap();
                assert!(matches!(
                    provenance,
                    Some(ReportProvenance::DispatchResult {
                        phase: crate::report_recovery::DispatchPhase::Ended,
                        ..
                    })
                ));
            }
            // A second termination of the same unit emits nothing twice.
            drop(deck.registry.begin_pane_close(&unit.delivery_pane_id));
            assert_eq!(
                rows.len(),
                crate::agent_pty::dispatch_lifecycle_tests::rows(&deck).len()
            );
        }
    }
}

/// Scenario: A dispatch that started nothing and one that started a unit each save their result as a file; with the row bound at 1, the evicted row's file stays, and its envelope names the unit and the caller. A dispatch is refused before anything starts when the store is full, and when the store's directory was refused, each with its own sentence.
#[spec("dispatch/return/018")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_018_dispatch_results_are_retained_and_refusals_come_first() {
    // SAFETY: a nextest process runs one test.
    unsafe { std::env::set_var(crate::report_recovery::ENV_OUTCOMES_MAX, "1") };
    let deck = deck();
    let mut ids = Vec::new();
    for (n, started) in [(0, false), (1, true)] {
        let room = crate::dispatch_return::admit_dispatch_result(&deck.registry).unwrap();
        let text = format!("result {n} RESULT-SENTINEL-{n}");
        let record = crate::dispatch_return::retain_dispatch_result(
            &deck.registry,
            room,
            "ret018-caller",
            "gone",
            &format!("ret018-unit-{n}"),
            &text,
            started,
            started.then(|| "unit-ret018".to_string()),
        )
        .await
        .unwrap();
        let reply = deck.registry.inbox().reserve_reply().unwrap();
        crate::dispatch_return::post_dispatch_result(
            &deck.registry,
            reply,
            "ret018-caller",
            "gone",
            &format!("ret018-unit-{n}"),
            started,
            &text,
            Some(record.clone()),
        );
        ids.push(record.report_id);
    }
    let outcomes = deck.registry.inbox().deck_outcomes().unwrap();
    assert_eq!(outcomes.outcomes.len(), 1);
    assert_eq!(outcomes.overflow, 1);
    let evicted = deck
        .store
        .dir()
        .join(crate::retained_store::own_name(&ids[0]));
    let file = std::fs::read(&evicted).expect("the evicted row's file stays");
    let envelope = crate::retained_store::read_attribution(&file[..]).unwrap();
    assert_eq!(
        envelope.producer_supplied.unit_name.as_deref(),
        Some("ret018-unit-0")
    );
    assert_eq!(envelope.recipient_agent_id, "gone");
    assert_eq!(
        envelope.producer_supplied.recipient_pane_id,
        "ret018-caller"
    );

    let full = deck_with(StoreConfig {
        max_bytes: 10,
        max_files: 100,
        fallback_slots: 16,
    });
    let (code, message) = crate::dispatch_return::admit_dispatch_result(&full.registry)
        .err()
        .expect("a full store refuses");
    assert_eq!(code, "report_store_full");
    assert!(message.contains("store is full") && message.ends_with("nothing was started"));

    let hostile = crate::test_temp::tempdir().unwrap();
    std::fs::create_dir(hostile.path().join("retained-reports")).unwrap();
    std::fs::set_permissions(
        hostile.path().join("retained-reports"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let refused = Arc::new(AgentPtyRegistry::new());
    refused.inbox().set_retained_store(RetainedStore::open(
        hostile.path(),
        StoreConfig::default(),
        Now::current(),
    ));
    let (code, message) = crate::dispatch_return::admit_dispatch_result(&refused)
        .err()
        .expect("a refused directory refuses");
    assert_eq!(code, "report_save_failed");
    assert!(message.contains("cannot be used") && message.ends_with("nothing was started"));
}

/// Scenario: A `dispatch ended` notice posted to a live caller is set aside as a row, with its summary and no file, when the caller exits before acknowledging it; acknowledged first, it leaves no row.
#[spec("dispatch/return/023")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_023_an_unacknowledged_dispatch_ended_notice_is_salvaged() {
    for acked in [false, true] {
        let deck = deck();
        let caller_agent = spawn_cat(&deck.registry, "ret023-caller");
        let unit = dispatch_single(
            &deck,
            &format!("ret023-{acked}"),
            "ret023-caller",
            &caller_agent,
        )
        .await;
        kill_child(&deck.registry, &unit.delivery_agent_id);
        eventually("the notice is posted", || {
            ended_notices(&deck, "ret023-caller") == 1
        })
        .await;
        if acked {
            let id = deck
                .registry
                .inbox()
                .list("ret023-caller")
                .into_iter()
                .find(|m| m.subject == "dispatch ended")
                .unwrap()
                .id;
            assert_eq!(
                deck.registry.inbox().ack_in_store_for_test(
                    id.as_str(),
                    "ret023-caller",
                    &caller_agent
                ),
                crate::inbox::AckOutcome::Acknowledged
            );
        }
        kill_child(&deck.registry, &caller_agent);
        eventually("the caller's exit is settled", || {
            deck.registry
                .inbox()
                .read_reports(|r| r.accepted.is_empty())
                .unwrap()
        })
        .await;
        let rows = rows(&deck);
        if acked {
            assert!(rows.is_empty(), "{rows:?}");
        } else {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].phase, OutcomePhase::DispatchEnded);
            assert_eq!(rows[0].storage, OutcomeStorage::None);
            assert!(rows[0].summary.contains("without reporting"));
        }
    }
}

/// Scenario: Fallback slots, with the slot knob at 2. Two dispatches whose saves fail are delivered with the held-in-memory line, rows set aside for them read `storage: memory`, and their bodies are held under their report ids. A third is refused before anything starts with the unsaved-results sentence. A retry saves them: rows read `artifact`, slots are free and a new dispatch is admitted. With no save failing, two dispatches in flight make a third refused with the in-flight sentence, which names no disk failure; one in flight and one held name both; no refusal tells anyone to restart.
#[spec("dispatch/return/024")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn return_024_fallback_slots_hold_results_and_word_their_refusals_apart() {
    let config = StoreConfig {
        max_bytes: crate::retained_store::DEFAULT_MAX_BYTES,
        max_files: 100,
        fallback_slots: 2,
    };
    let deck = deck_with(config.clone());
    deck.store.fail_writes_for_test(true);
    let mut held = Vec::new();
    for n in 0..2 {
        let room = crate::dispatch_return::admit_dispatch_result(&deck.registry).unwrap();
        let record = crate::dispatch_return::retain_dispatch_result(
            &deck.registry,
            room,
            "ret024-caller",
            "gone",
            &format!("ret024-{n}"),
            &format!("held result {n}"),
            n == 1,
            None,
        )
        .await
        .unwrap();
        assert_eq!(record.storage, OutcomeStorage::Memory);
        let reply = deck.registry.inbox().reserve_reply().unwrap();
        crate::dispatch_return::post_dispatch_result(
            &deck.registry,
            reply,
            "ret024-caller",
            "gone",
            &format!("ret024-{n}"),
            n == 1,
            &format!("held result {n}"),
            Some(record.clone()),
        );
        held.push(record.report_id);
    }
    assert!(
        rows(&deck)
            .iter()
            .all(|r| r.storage == OutcomeStorage::Memory)
    );
    let bodies = deck
        .registry
        .inbox()
        .read_reports(|r| {
            held.iter()
                .map(|id| String::from_utf8_lossy(&r.fallback[id].contents).into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert!(bodies[0].contains("held result 0") && bodies[1].contains("held result 1"));
    let (code, message) = crate::dispatch_return::admit_dispatch_result(&deck.registry)
        .err()
        .expect("the third is refused");
    assert_eq!(code, "dispatch_slots_full");
    assert!(
        message.contains("2 earlier results could not be saved"),
        "{message}"
    );
    assert!(!message.contains("still starting"), "{message}");

    deck.store.fail_writes_for_test(false);
    deck.registry.inbox().retry_fallback_bodies(&deck.store);
    assert!(
        rows(&deck)
            .iter()
            .all(|r| r.storage == OutcomeStorage::Artifact)
    );
    let again = crate::dispatch_return::admit_dispatch_result(&deck.registry).unwrap();
    drop(again);

    let busy = deck_with(config);
    let a = crate::dispatch_return::admit_dispatch_result(&busy.registry).unwrap();
    let b = crate::dispatch_return::admit_dispatch_result(&busy.registry).unwrap();
    let (_, message) = crate::dispatch_return::admit_dispatch_result(&busy.registry)
        .err()
        .unwrap();
    assert!(
        message.contains("2 dispatches are still starting"),
        "{message}"
    );
    assert!(!message.contains("saved to disk") && !message.to_lowercase().contains("restart"));
    drop(a);
    busy.store.fail_writes_for_test(true);
    let _ = crate::dispatch_return::retain_dispatch_result(
        &busy.registry,
        b,
        "c",
        "gone",
        "n",
        "t",
        true,
        None,
    )
    .await;
    let _c = crate::dispatch_return::admit_dispatch_result(&busy.registry).unwrap();
    let (_, message) = crate::dispatch_return::admit_dispatch_result(&busy.registry)
        .err()
        .unwrap();
    assert!(
        message.contains("still starting") && message.contains("could not be saved"),
        "{message}"
    );
}

/// A dispatch result whose file cannot be written, held in memory and posted
/// to `caller` (`gone` for a caller that is not running). Answers its report
/// id.
async fn held_result(deck: &Deck, caller_pane: &str, caller_agent: &str, name: &str) -> String {
    deck.store.fail_writes_for_test(true);
    let room = crate::dispatch_return::admit_dispatch_result(&deck.registry).unwrap();
    let record = crate::dispatch_return::retain_dispatch_result(
        &deck.registry,
        room,
        caller_pane,
        caller_agent,
        name,
        &format!("held result {name}"),
        true,
        None,
    )
    .await
    .unwrap();
    assert_eq!(record.storage, OutcomeStorage::Memory);
    let reply = deck.registry.inbox().reserve_reply().unwrap();
    crate::dispatch_return::post_dispatch_result(
        &deck.registry,
        reply,
        caller_pane,
        caller_agent,
        name,
        true,
        &format!("held result {name}"),
        Some(record.clone()),
    );
    record.report_id
}

fn held_bodies(deck: &Deck) -> usize {
    deck.registry
        .inbox()
        .read_reports(|r| r.fallback.len())
        .unwrap()
}

/// Scenario: A dispatch result whose file cannot be written is held in memory, and keeps the lifetime a saved file would have on every path. Its caller already gone, it enters recovery at once: with the disk still broken it is held after six days and released, room and slot, after seven. Acknowledged, or set aside, and then saved by a retry, its file is acknowledged or recovered rather than pinned, a restart would adopt it as such, and the sweep removes it 24 hours or seven days after that event, not after the retry. Acknowledged, or its caller exiting, while a retry is writing it, the same. Acknowledged and retried with the disk still broken, it is released after 24 hours.
#[spec("dispatch/return/024")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_024_a_held_result_keeps_its_lifetime_on_every_path() {
    use crate::retained_store::{AdoptedState, Now, OwnState, parse_adopted_name};
    const HOUR: Duration = Duration::from_secs(60 * 60);
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    let adopted_letter = |deck: &Deck, id: &str| -> Vec<char> {
        super::report_tests::adopted_after_restart(deck)
            .iter()
            .filter_map(|name| parse_adopted_name(name))
            .filter(|(report, _, _)| *report == id)
            .map(|(_, _, state)| state.letter())
            .collect()
    };

    // The caller is gone, and the disk stays broken.
    let deck = self::deck();
    let baseline = deck.store.counts().unwrap();
    let start = Now::current();
    let _id = held_result(&deck, "l024a-caller", "gone", "l024a").await;
    assert_eq!(rows(&deck).len(), 1, "set aside at once");
    deck.registry
        .inbox()
        .retry_fallback_bodies_at(&deck.store, start.plus(DAY * 6));
    assert_eq!(held_bodies(&deck), 1, "held after six days");
    deck.registry
        .inbox()
        .retry_fallback_bodies_at(&deck.store, start.plus(DAY * 7 + HOUR));
    assert_eq!(held_bodies(&deck), 0, "released after seven");
    assert_eq!(deck.store.counts().unwrap(), baseline, "its room and slot");

    // Acknowledged, or set aside, then saved by a retry.
    for acknowledged in [true, false] {
        let deck = self::deck();
        let baseline = deck.store.counts().unwrap();
        let (pane, agent) = if acknowledged {
            ("l024b-caller", spawn_cat(&deck.registry, "l024b-caller"))
        } else {
            ("l024c-caller", "gone".to_string())
        };
        let id = held_result(&deck, pane, &agent, "l024b").await;
        let event = Now::current();
        if acknowledged {
            deck.registry.inbox().test_take_acked(pane, &agent);
        }
        deck.store.fail_writes_for_test(false);
        deck.registry
            .inbox()
            .retry_fallback_bodies_at(&deck.store, event.plus(HOUR));
        assert_eq!(held_bodies(&deck), 0, "saved");
        let (letter, retention) = if acknowledged {
            assert!(matches!(
                deck.store.own_state(&id),
                Some(OwnState::Acknowledged(_))
            ));
            (AdoptedState::Acknowledged.letter(), DAY)
        } else {
            assert!(matches!(
                deck.store.own_state(&id),
                Some(OwnState::Recovered(_))
            ));
            (AdoptedState::Recovered.letter(), DAY * 7)
        };
        assert_eq!(adopted_letter(&deck, &id), vec![letter]);
        deck.store.sweep(event.plus(retention - HOUR));
        assert_eq!(deck.store.counts().unwrap().own_files, 1, "kept until then");
        deck.store.sweep(event.plus(retention + HOUR));
        assert_eq!(
            deck.store.counts().unwrap(),
            baseline,
            "removed {retention:?} after the event, not after the retry"
        );
    }

    // Acknowledged, or the caller exiting, while a retry is writing it.
    for exits in [false, true] {
        let deck = self::deck();
        let pane = format!("l024d{}-caller", u8::from(exits));
        let agent = spawn_cat(&deck.registry, &pane);
        let id = held_result(&deck, &pane, &agent, "l024d").await;
        deck.store.fail_writes_for_test(false);
        let (reached, held) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        *deck.registry.inbox().hold_fallback_retry.lock().unwrap() = Some((reached, wait));
        let retry = {
            let registry = Arc::clone(&deck.registry);
            let store = Arc::clone(&deck.store);
            tokio::task::spawn_blocking(move || {
                registry.inbox().retry_fallback_bodies(&store);
            })
        };
        tokio::task::spawn_blocking(move || held.recv().unwrap())
            .await
            .unwrap();
        if exits {
            kill_child(&deck.registry, &agent);
            eventually("the result is set aside", || rows(&deck).len() == 1).await;
        } else {
            deck.registry.inbox().test_take_acked(&pane, &agent);
        }
        release.send(()).unwrap();
        retry.await.unwrap();
        let state = deck.store.own_state(&id);
        if exits {
            assert!(matches!(state, Some(OwnState::Recovered(_))), "{state:?}");
        } else {
            assert!(
                matches!(state, Some(OwnState::Acknowledged(_))),
                "{state:?}"
            );
        }
    }

    // Acknowledged, and the disk stays broken.
    let deck = self::deck();
    let baseline = deck.store.counts().unwrap();
    let agent = spawn_cat(&deck.registry, "l024e-caller");
    let _id = held_result(&deck, "l024e-caller", &agent, "l024e").await;
    let event = Now::current();
    deck.registry
        .inbox()
        .test_take_acked("l024e-caller", &agent);
    deck.registry
        .inbox()
        .retry_fallback_bodies_at(&deck.store, event.plus(HOUR));
    assert_eq!(held_bodies(&deck), 1);
    deck.registry
        .inbox()
        .retry_fallback_bodies_at(&deck.store, event.plus(DAY + HOUR));
    assert_eq!(held_bodies(&deck), 0);
    assert_eq!(deck.store.counts().unwrap(), baseline);
}

/// Scenario: A held dispatch result is handed to its live caller once, the only hand-over it gets, and that hand-over is reported not handed: it is set aside, and its seven days start. The caller's commit then acknowledges it late, which starts 24 hours instead: with the disk still broken, the body is released a day after that acknowledgement, not seven.
#[spec("dispatch/return/024")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_024_a_late_acknowledgement_shortens_a_held_results_lifetime() {
    use crate::retained_store::Now;
    // SAFETY: a nextest process runs one test.
    unsafe { std::env::set_var(crate::inbox::ENV_MAX_DELIVER, "1") };
    let deck = deck();
    let baseline = deck.store.counts().unwrap();
    let agent = spawn_cat(&deck.registry, "l024f-caller");
    let _id = held_result(&deck, "l024f-caller", &agent, "l024f").await;
    let hub = deck.registry.inbox();
    let hook = hub.test_hook_fetch("l024f-caller", &agent, None);
    let id = hook.ids().into_iter().next().expect("handed over once");
    hub.test_hook_not_handed(&hook);
    assert_eq!(rows(&deck).len(), 1, "out of hand-overs, it is set aside");
    // The caller's commit says it got it after all.
    let late = Now::current();
    hub.test_hook_committed(&hook, &[id.as_str()]);
    hub.test_hook_close(hook);
    assert_eq!(
        rows(&deck)[0].delivery,
        crate::report_recovery::OutcomeDelivery::AcknowledgedLate
    );
    hub.retry_fallback_bodies_at(&deck.store, late.plus(Duration::from_secs(25 * 60 * 60)));
    assert_eq!(held_bodies(&deck), 0, "released a day after the late ack");
    assert_eq!(deck.store.counts().unwrap(), baseline);
}

/// [`dispatch_single`] as a task, so a test can pause it.
fn dispatch_single_task(
    deck: &Deck,
    name: &str,
    caller_pane: &str,
    caller_agent: &str,
) -> tokio::task::JoinHandle<Result<SpawnHandle, crate::spawn::SpawnError>> {
    let registry = Arc::clone(&deck.registry);
    let dir = deck.root.path().join(name);
    let req = SpawnRequest {
        task_name: format!("dispatch-{name}"),
        working_dir: dir.to_string_lossy().into_owned(),
        command: None,
        prompt: format!("task for {name}"),
        resolved_target: Some(SpawnTarget::SingleAgent {
            command: Some("cat".to_string()),
            inherited: None,
        }),
        compose_orchestrator_context: None,
        seed_reason: None,
    };
    let origin = origin(deck, name, caller_pane, caller_agent);
    let caller = caller(caller_pane, caller_agent, name);
    tokio::spawn(async move {
        crate::spawn::spawn_dispatched_unit(
            req, &registry, &Silent, None, true, None, origin, caller,
        )
        .await
    })
}

/// Scenario: (e) the unit's terminal generation exits, and (f) its pane is marked closing, while the dispatch is paused before registration. Registration then records the unit's end (`Exited`, `Closed`), tells the live caller once with `dispatch ended`, and inserts no route; the generation is indexed to the unit.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_e_f_a_unit_that_ends_before_registration_is_recorded_ended() {
    for closing in [false, true] {
        let deck = deck();
        let caller_agent = spawn_cat(&deck.registry, "r027-caller");
        let (reached, release) = deck.registry.pause_at_for_test("dispatch-publication");
        let task = dispatch_single_task(
            &deck,
            &format!("r027-{closing}"),
            "r027-caller",
            &caller_agent,
        );
        reached.await.unwrap();
        let (pane, agent) = {
            let inner = deck.registry.inner.lock();
            inner
                .agents
                .iter()
                .find(|(_, a)| {
                    a.pane_id_env
                        .as_deref()
                        .is_some_and(|p| p.starts_with("sched-dispatch-r027"))
                })
                .map(|(id, a)| (a.pane_id_env.clone().unwrap(), id.clone()))
                .unwrap()
        };
        if closing {
            drop(deck.registry.begin_pane_close(&pane));
        } else {
            kill_child(&deck.registry, &agent);
            eventually("the unit's terminal exited", || {
                deck.registry
                    .agent_record_any(&agent)
                    .is_some_and(|r| r.crashed == Some(true))
            })
            .await;
        }
        release.send(()).unwrap();
        // The seed then has no live agent to go to, so the dispatch itself
        // answers an error; what registration recorded is what is asserted.
        let _ = task.await.unwrap();
        let unit_id = deck
            .registry
            .inbox()
            .read_reports(|r| r.units.unit_of(&pane, &agent).map(str::to_string))
            .unwrap()
            .expect("the generation is indexed to its unit");
        assert_eq!(
            unit_entry(&deck, &unit_id),
            Some(UnitEntry::Ended {
                how: if closing {
                    EndedHow::Closed
                } else {
                    EndedHow::Exited
                }
            })
        );
        assert_eq!(deck.registry.outstanding_dispatch_returns(), 0, "no route");
        assert_eq!(ended_notices(&deck, "r027-caller"), 1);
    }
}

/// Scenario: (i) a second role generation of a dispatched orchestration instance is reserved with origin `Direct`, through the spawn options: the reservation logs the conflict and pins its origin as `Conflict`, never either origin.
#[spec("dispatch/return/027")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_027_i_a_contradictory_origin_is_pinned_as_a_conflict() {
    let deck = deck();
    let caller_agent = spawn_cat(&deck.registry, "r027i-caller");
    let unit = dispatch_orchestration(&deck, "r027i", "r027i-caller", &caller_agent).await;
    let instance = unit.orchestration_id.clone().unwrap();
    let origin_of = |agent: &str| {
        deck.registry
            .inner
            .lock()
            .agents
            .get(agent)
            .and_then(|a| a.attest_facts.seat.clone())
    };
    let seat = origin_of(&unit.delivery_agent_id).expect("an orchestrator seat");
    assert_eq!(
        seat.origin,
        Some(crate::hook_provenance::InstanceOrigin::Dispatched)
    );
    assert!(seat.is_orchestrator);
    let membership = TabMembership::Orchestration {
        name: "r027i".to_string(),
        role_index: 2,
        role_name: "intruder".to_string(),
        is_start_role: false,
        orchestration_cwd: None,
        display_title: None,
        orchestration_id: Some(instance.clone()),
    };
    let intruder = deck
        .registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "r027i-intruder".to_string(),
            )],
            tab_membership: Some(membership),
            origin: Some(crate::hook_provenance::InstanceOrigin::Direct),
            ..SpawnOptions::default()
        })
        .unwrap();
    assert_eq!(
        origin_of(&intruder).and_then(|s| s.origin),
        Some(crate::hook_provenance::InstanceOrigin::Conflict)
    );
}

/// Scenario: With the tombstone knob at 1 (a daemon-wide bound of 16 units), a dispatched unit whose terminal generation is still registered keeps its entry and index while 32 other units' entries are evicted; a unit a publication names is kept too. Once its pane is closed and its record removed, the next insertion evicts it.
#[spec("dispatch/return/028")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn return_028_the_index_outlives_no_attesting_generation() {
    // SAFETY: a nextest process runs one test.
    unsafe { std::env::set_var(crate::report_recovery::ENV_TOMBSTONES_MAX, "1") };
    let deck = deck();
    let caller_agent = spawn_cat(&deck.registry, "r028-caller");
    let unit = dispatch_single(&deck, "r028", "r028-caller", &caller_agent).await;
    let unit_id = unit.unit_id.clone().unwrap();
    kill_child(&deck.registry, &unit.delivery_agent_id);
    eventually("ended", || unit_entry(&deck, &unit_id).is_some()).await;
    let others: Vec<(String, String, String)> = (0..32)
        .map(|n| {
            (
                format!("r028-p{n}"),
                format!("gone-{n}"),
                format!("u-r028-{n}"),
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
    assert!(
        unit_entry(&deck, &unit_id).is_some(),
        "kept: its generation is registered"
    );
    assert!(
        unit_entry(&deck, "u-r028-0").is_none(),
        "an unprotected unit is evicted"
    );
    deck.registry.close_agent(&unit.delivery_agent_id).unwrap();
    deck.registry.inbox().insert_ended_units_for_test(
        deck.registry.as_ref(),
        &[("r028-last", "gone-last", "u-r028-last")],
    );
    assert!(
        unit_entry(&deck, &unit_id).is_none(),
        "evictable once no generation attests"
    );
}
