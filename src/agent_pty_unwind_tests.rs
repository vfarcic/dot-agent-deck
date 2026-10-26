//! PRD #1666 part 3, round 5 (C4b): a destructor that reaches a ranked lock
//! never panics during an unwind, which would abort the daemon.
//!
//! Each case runs in a child process: the outer test re-executes this test
//! binary filtered to itself with [`CASE`] set, and the child arms one
//! destructor, poisons its lock or holds a higher rank, and panics with an
//! injected payload. A destructor that panicked during that unwind would abort
//! the child, which the outer test reads as a failed status (`SIGABRT`)
//! instead of hanging or failing the runner. The child also installs a
//! subscriber that panics on any `tracing` event for the unwind, so a log call
//! made there is a second panic too.

use std::sync::Arc;

use spec::spec;

use super::*;
use crate::lock_rank::{LEAF, RankedMutex};

const CASE: &str = "DOT_AGENT_DECK_TEST_UNWIND_CASE";

/// What the child does to the destructor's lock before the unwind.
#[derive(Clone, Copy)]
enum Fault {
    /// A thread panicked holding it.
    Poisoned,
    /// The unwinding thread holds a rank at or above it (debug builds).
    RankHeld,
}

impl Fault {
    fn name(self) -> &'static str {
        match self {
            Fault::Poisoned => "poisoned",
            Fault::RankHeld => "rank-held",
        }
    }
}

/// A subscriber that panics on any event.
struct PanicOnEvent;

impl tracing::Subscriber for PanicOnEvent {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        panic!("a tracing event during the unwind: {:?}", event.metadata());
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn poison_registry(registry: &Arc<AgentPtyRegistry>) {
    let registry = Arc::clone(registry);
    let _ = std::thread::spawn(move || {
        let _held = registry.inner.lock();
        panic!("poison the registry lock on purpose (test)");
    })
    .join();
}

fn poison_tracker(registry: &Arc<AgentPtyRegistry>) {
    let registry = Arc::clone(registry);
    let _ = std::thread::spawn(move || {
        let _held = registry.delegations.lock();
        panic!("poison the delegation tracker on purpose (test)");
    })
    .join();
}

/// In the child: arm a destructor with `arm`, apply `fault` with `poison`,
/// then unwind through it with an injected payload, and check the payload is
/// the one that escaped. The registry is leaked: its own `Drop` is the
/// documented boundary that still panics on a poisoned lock, and the guards
/// borrow it.
fn unwind_through<G>(
    fault: Fault,
    arm: impl FnOnce(&'static Arc<AgentPtyRegistry>) -> G,
    poison: impl FnOnce(&Arc<AgentPtyRegistry>),
) -> &'static Arc<AgentPtyRegistry> {
    let registry: &'static Arc<AgentPtyRegistry> =
        Box::leak(Box::new(Arc::new(AgentPtyRegistry::new())));
    let leaf = RankedMutex::<LEAF, ()>::new(());
    let armed = arm(registry);
    if let Fault::Poisoned = fault {
        poison(registry);
    }
    let capture = crate::test_isolation::capture_tracing_on_this_thread(PanicOnEvent);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Declared first, so it is dropped last: the destructor runs while
        // the higher rank is still held.
        let _held = matches!(fault, Fault::RankHeld).then(|| leaf.lock_recovering());
        let _armed = armed;
        std::panic::panic_any("injected-unwind-payload");
    }));
    drop(capture);
    let payload = unwound.expect_err("the injected panic");
    assert_eq!(
        payload.downcast_ref::<&str>(),
        Some(&"injected-unwind-payload"),
        "the injected payload escaped, not a second panic"
    );
    registry
}

/// The outer half: run `test` (this module's test of that name) in a child
/// with `case`, and require that it passed rather than aborted.
fn run_child(test: &str, case: &str) {
    let name = format!("agent_pty::unwind_tests::{test}");
    let output =
        std::process::Command::new(std::env::current_exe().expect("the test binary has a path"))
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CASE, case)
            .output()
            .expect("re-execute the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "case {case}: the child did not survive the unwind ({:?}):\n{stdout}\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("1 passed"),
        "case {case}: the child did not run {name}:\n{stdout}\n{stderr}"
    );
}

/// The child's case, or `None` in the outer run.
fn child_case() -> Option<String> {
    std::env::var(CASE).ok()
}

fn outer(test: &str) {
    for fault in [Fault::Poisoned, Fault::RankHeld] {
        if cfg!(not(debug_assertions)) && matches!(fault, Fault::RankHeld) {
            continue;
        }
        run_child(test, fault.name());
    }
}

fn fault_of(case: &str) -> Fault {
    match case {
        "poisoned" => Fault::Poisoned,
        _ => Fault::RankHeld,
    }
}

/// Scenario: A respawn's in-flight ticket drops during an unwind, once with the registry lock poisoned and once with a higher rank held. The child is not aborted: poisoned, the ticket still settles its respawn (recovering, as it always has); with the rank held, it skips.
#[spec("orchestration/work-done/056")]
#[test]
fn work_done_056_respawn_ticket_drop_survives_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_056_respawn_ticket_drop_survives_an_unwind");
    };
    let fault = fault_of(&case);
    let registry = unwind_through(
        fault,
        |registry| {
            registry.inner.lock().respawns_in_flight += 1;
            RespawnTicket {
                registry: Arc::as_ref(registry),
                agent_id: "unwind-agent".into(),
                removal: Arc::default(),
            }
        },
        poison_registry,
    );
    let in_flight = registry.inner.lock_recovering().respawns_in_flight;
    match fault {
        Fault::Poisoned => assert_eq!(in_flight, 0, "recovered and settled"),
        Fault::RankHeld => assert_eq!(in_flight, 1, "skipped"),
    }
}

/// Scenario: A commission-dispatch guard drops during an unwind with the delegation tracker poisoned (where it used to panic) or a higher rank held. The child survives, and the guard skipped: its in-flight entry is left.
#[spec("orchestration/work-done/056")]
#[test]
fn work_done_056_commission_dispatch_in_flight_drop_survives_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_056_commission_dispatch_in_flight_drop_survives_an_unwind");
    };
    let registry = unwind_through(
        fault_of(&case),
        |registry| {
            registry
                .delegations
                .lock()
                .commission_dispatches_in_flight
                .entry("unwind-worker".into())
                .or_default()
                .insert(7, None);
            CommissionDispatchInFlight {
                registry: Arc::clone(registry),
                worker_pane_id: "unwind-worker".into(),
                id: 7,
            }
        },
        poison_tracker,
    );
    assert!(
        registry
            .delegations
            .lock_recovering()
            .commission_dispatches_in_flight
            .contains_key("unwind-worker"),
        "skipped, so the entry stays"
    );
}

/// Scenario: A spawn reservation with an abandoned turn drops during an unwind, once with the registry lock poisoned and once with a higher rank held. The child survives; the reservation skips the registry, as it always has on poison, and so takes no turn to the hub.
#[spec("orchestration/work-done/057")]
#[test]
fn work_done_057_spawn_reservation_drop_survives_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_057_spawn_reservation_drop_survives_an_unwind");
    };
    let registry = unwind_through(
        fault_of(&case),
        |registry| {
            let mut inner = registry.inner.lock();
            inner
                .pending_spawns
                .insert("unwind-agent".into(), Some("unwind-pane".into()));
            inner
                .pending_turned
                .insert("unwind-agent".into(), "unwind-pane".into());
            drop(inner);
            SpawnReservation {
                registry: Arc::as_ref(registry),
                id: Some("unwind-agent".into()),
                #[cfg(unix)]
                prior_binding: None,
            }
        },
        poison_registry,
    );
    assert!(
        registry
            .inner
            .lock_recovering()
            .pending_spawns
            .contains_key("unwind-agent"),
        "skipped: the pending spawn stays, bounded to one"
    );
}

/// Scenario: Abandoned turns are released by a destructor during an unwind with the inbox hub poisoned, and again with a higher rank held. The child survives; with the hub poisoned the turn is still taken to the hub, which recovers as every hub path does.
#[spec("orchestration/work-done/057")]
#[test]
fn work_done_057_release_abandoned_turns_survives_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_057_release_abandoned_turns_survives_an_unwind");
    };
    let fault = fault_of(&case);
    let registry = unwind_through(
        fault,
        |registry| {
            registry
                .inner
                .lock()
                .abandoned_turns
                .push(("unwind-pane".into(), "unwind-agent".into()));
            ReleaseAbandonedTurns(Arc::as_ref(registry))
        },
        |registry| registry.inbox().poison_for_test(),
    );
    let left = registry.inner.lock_recovering().abandoned_turns.len();
    match fault {
        Fault::Poisoned => assert_eq!(left, 0, "taken to the recovering hub"),
        Fault::RankHeld => assert_eq!(left, 1, "skipped"),
    }
}

/// Scenario: A directory-removal hold and a pane-cleanup hold drop during an unwind with the registry lock poisoned or a higher rank held. The child survives and both skip, leaving their hold, as they always have on poison.
#[spec("orchestration/work-done/057")]
#[test]
fn work_done_057_removal_and_cleanup_holds_drop_survives_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_057_removal_and_cleanup_holds_drop_survives_an_unwind");
    };
    let registry = unwind_through(
        fault_of(&case),
        |registry| {
            let mut inner = registry.inner.lock();
            inner.removal_holds.push(PathBuf::from("/unwind/dir"));
            inner.cleanup_holds.insert("unwind-pane".into());
            drop(inner);
            (
                DirRemovalHold {
                    registry: Arc::as_ref(registry),
                    dir: PathBuf::from("/unwind/dir"),
                },
                PaneCleanupHold {
                    registry: Arc::clone(registry),
                    pane_id: "unwind-pane".into(),
                },
            )
        },
        poison_registry,
    );
    let inner = registry.inner.lock_recovering();
    assert_eq!(inner.removal_holds.len(), 1);
    assert!(inner.cleanup_holds.contains("unwind-pane"));
}

/// Scenario: A generation close guard and an instance close guard drop during an unwind, once with the registry lock poisoned and once with a higher rank held. The child survives: poisoned, both recover and clear their closing mark, as they always have; with the rank held, both skip.
#[spec("orchestration/work-done/057")]
#[test]
fn work_done_057_close_guards_drop_survives_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_057_close_guards_drop_survives_an_unwind");
    };
    let fault = fault_of(&case);
    let registry = unwind_through(
        fault,
        |registry| {
            let mut inner = registry.inner.lock();
            inner.closing_generations.insert("unwind-agent".into());
            inner.closing_instances.insert("unwind-instance".into());
            drop(inner);
            (
                GenerationCloseGuard {
                    registry: Arc::clone(registry),
                    agent_id: "unwind-agent".into(),
                },
                InstanceCloseGuard {
                    registry: Arc::clone(registry),
                    instance: "unwind-instance".into(),
                    finished: false,
                },
            )
        },
        poison_registry,
    );
    let inner = registry.inner.lock_recovering();
    let marked = (
        inner.closing_generations.contains("unwind-agent"),
        inner.closing_instances.contains("unwind-instance"),
    );
    match fault {
        Fault::Poisoned => assert_eq!(marked, (false, false), "recovered"),
        Fault::RankHeld => assert_eq!(marked, (true, true), "skipped"),
    }
}

/// Scenario: A reply-room reservation drops during an unwind with the inbox hub poisoned, and again with a higher rank held. The child survives: poisoned, the room is given back through the recovering hub; with the rank held, the room stays reserved (the documented bounded leak).
#[spec("orchestration/work-done/056")]
#[test]
fn work_done_056_reply_reservation_drop_survives_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_056_reply_reservation_drop_survives_an_unwind");
    };
    let fault = fault_of(&case);
    let registry = unwind_through(
        fault,
        |registry| registry.inbox().reserve_reply().expect("room"),
        |registry| registry.inbox().poison_for_test(),
    );
    let reserved = registry.inbox().reserved_replies();
    match fault {
        Fault::Poisoned => assert_eq!(reserved, 0, "released through the recovering hub"),
        Fault::RankHeld => assert_eq!(reserved, 1, "skipped"),
    }
}

/// The destructor bodies of `state.rs`'s `PointerQueueClock` and
/// `AbandonPostOnDrop`, and the attach stream's `ViewerGuard`: each calls one
/// registry or hub helper.
struct HelperDrops {
    registry: Arc<AgentPtyRegistry>,
}

impl Drop for HelperDrops {
    fn drop(&mut self) {
        self.registry
            .dequeue_delegation_pointer_write_in_drop("unwind-worker", 1);
        self.registry.inbox().abandon_post("unwind-post");
        self.registry.release_viewer("unwind-agent", "unwind-token");
    }
}

/// Scenario: The helpers three destructors call — the pointer-queue dequeue, a pending post's abandonment and a viewer's release — run from a `Drop` during an unwind with the tracker, the hub and the registry lock all poisoned, and again with a higher rank held. The child survives: the tracker and the registry are skipped where they used to panic, and the hub recovers.
#[spec("orchestration/work-done/057")]
#[test]
fn work_done_057_pointer_post_and_viewer_drops_survive_an_unwind() {
    let Some(case) = child_case() else {
        return outer("work_done_057_pointer_post_and_viewer_drops_survive_an_unwind");
    };
    unwind_through(
        fault_of(&case),
        |registry| HelperDrops {
            registry: Arc::clone(registry),
        },
        |registry| {
            poison_tracker(registry);
            registry.inbox().poison_for_test();
            poison_registry(registry);
        },
    );
}
