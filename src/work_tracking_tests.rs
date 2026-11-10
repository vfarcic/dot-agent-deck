//! PRD #1889: the daemon's work bookkeeping — which work each spawn, end,
//! report and close is written against.

use super::*;
use crate::agent_pty::{AgentPtyRegistry, ChildRole, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use crate::deck_identity::DeckId;
use crate::work_journal::{AttemptId, ENV_WORK_ID, JournalOptions, Line, read_journal};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn journal(state: &Path, run: &str) -> WorkJournal {
    WorkJournal::try_open_with(
        state,
        run,
        &DeckId::generate(),
        JournalOptions {
            home: PathBuf::from("/home/someone"),
            ..JournalOptions::default()
        },
    )
    .unwrap()
}

fn tracker(state: &Path) -> WorkTracker {
    let tracker = WorkTracker::default();
    tracker.install(journal(state, "run1"));
    tracker
}

/// Every line, once the tracker's journal has written what it was handed.
fn lines(tracker: &WorkTracker, state: &Path) -> Vec<Line> {
    if let Some(journal) = tracker.journal() {
        journal.flush();
    }
    read_journal(state).unwrap().lines
}

fn kinds_of(lines: &[Line], work: &WorkId) -> Vec<&'static str> {
    lines
        .iter()
        .filter(|l| l.event.work() == work)
        .map(|l| l.event.kind())
        .collect()
}

fn orchestration(instance: &str, role: &str) -> TabMembership {
    TabMembership::Orchestration {
        name: "tdd".into(),
        role_index: 0,
        role_name: role.into(),
        is_start_role: role == "orchestrator",
        orchestration_cwd: Some("/home/someone/code/x".into()),
        display_title: None,
        orchestration_id: Some(instance.into()),
    }
}

#[test]
fn termination_maps_to_the_attempts_end() {
    let exited = Termination::PaneCommandExited {
        code: 3,
        role: ChildRole::PaneCommand,
        reported_by: crate::inbox_display::TerminationSource::Deck,
    };
    assert_eq!(
        AttemptEnd::of(&exited, false),
        AttemptEnd {
            how: EndHow::Exited,
            code: Some(3),
            cause: None
        }
    );
    // A close while the daemon is going down is the daemon interrupting it.
    assert_eq!(
        AttemptEnd::of(&Termination::Closed, true),
        AttemptEnd {
            how: EndHow::Interrupted,
            code: None,
            cause: Some(EndCause::DaemonShutdown)
        }
    );
    let how = |t: Termination| AttemptEnd::of(&t, false).how;
    assert_eq!(how(Termination::Closed), EndHow::Closed);
    assert_eq!(how(Termination::Replaced), EndHow::Replaced);
    assert_eq!(
        how(Termination::ReplacementFailed),
        EndHow::ReplacementFailed
    );
    assert_eq!(how(Termination::ReplacementPending), EndHow::Unknown);
    assert_eq!(how(Termination::Unknown), EndHow::Unknown);
    // Whatever else shuts down, only a close reads as interrupted.
    assert_eq!(
        AttemptEnd::of(&Termination::Replaced, true).how,
        EndHow::Replaced
    );
}

#[test]
fn without_a_journal_nothing_is_minted_or_written() {
    let tracker = WorkTracker::default();
    assert!(tracker.mint().is_none());
    assert!(
        tracker
            .resolve_spawn(None, Some("3"), None, Some("/tmp"))
            .kept()
            .is_none()
    );
}

#[test]
fn a_pane_spawn_mints_a_pane_work_opened_at_its_first_attempt() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .resolve_spawn(None, Some("3"), None, Some("/home/someone/code/x"))
        .kept()
        .unwrap();
    assert!(
        lines(&tracker, state.path()).is_empty(),
        "nothing before the publish"
    );
    tracker.attempt_started(&work, "7", Some("3"), None);
    // A respawn of the pane asks for the same work and opens nothing new.
    assert_eq!(
        tracker
            .resolve_spawn(Some(&work), Some("3"), None, Some("/home/someone/code/x"))
            .kept(),
        Some(work.clone())
    );
    tracker.attempt_ended("7", AttemptEnd::of(&Termination::Replaced, false));
    tracker.attempt_started(&work, "8", Some("3"), None);
    tracker.pane_closed("3");
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &work),
        [
            "opened",
            "attempt_started",
            "attempt_ended",
            "attempt_started",
            "closed"
        ]
    );
    match &read[0].event {
        Event::Opened {
            launch,
            parent,
            refs,
            workspace,
            ..
        } => {
            assert_eq!(*launch, Launch::Pane);
            assert_eq!(*parent, None);
            assert_eq!(refs.pane.as_deref(), Some("3"));
            assert_eq!(workspace.as_ref().unwrap().as_str(), "~/code/x");
        }
        other => panic!("expected opened, got {other:?}"),
    }
    match &read[1].event {
        Event::AttemptStarted { attempt, agent, .. } => {
            assert_eq!(*attempt, AttemptId::new("run1", 7).unwrap());
            assert_eq!(*agent, 7);
        }
        other => panic!("expected attempt_started, got {other:?}"),
    }
    assert!(matches!(
        &read[4].event,
        Event::Closed {
            how: CloseHow::PaneClosed,
            ..
        }
    ));
    assert_eq!(
        tracker.work_of_pane("3"),
        None,
        "a closed pane is forgotten"
    );
}

#[test]
fn an_orchestrations_roles_are_attempts_of_one_work_under_its_parent() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let parent = tracker
        .open(Launch::ScheduleFire, None, Refs::default(), None)
        .unwrap();
    let orch = orchestration("orch-1", "orchestrator");
    let coder = orchestration("orch-1", "coder");
    let a = tracker
        .resolve_spawn(Some(&parent), Some("o-1"), Some(&orch), None)
        .kept()
        .unwrap();
    let b = tracker
        .resolve_spawn(Some(&parent), Some("o-2"), Some(&coder), None)
        .kept()
        .unwrap();
    assert_eq!(a, b);
    assert_ne!(a, parent);
    assert_eq!(tracker.orchestration_work("orch-1"), Some(a.clone()));
    tracker.attempt_started(&a, "1", Some("o-1"), Some("orchestrator"));
    tracker.attempt_started(&a, "2", Some("o-2"), Some("coder"));
    // A role pane's close does not close the orchestration's work.
    tracker.pane_closed("o-2");
    tracker.orchestration_ended("orch-1");
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &a),
        ["opened", "attempt_started", "attempt_started", "closed"]
    );
    let opened = read
        .iter()
        .find(|l| l.event.work() == &a && l.event.kind() == "opened")
        .unwrap();
    match &opened.event {
        Event::Opened {
            launch,
            parent: p,
            refs,
            workspace,
            ..
        } => {
            assert_eq!(*launch, Launch::Orchestration);
            assert_eq!(p.as_ref(), Some(&parent));
            assert_eq!(refs.orchestration_id.as_deref(), Some("orch-1"));
            assert_eq!(refs.orchestration.as_deref(), Some("tdd"));
            assert_eq!(workspace.as_ref().unwrap().as_str(), "~/code/x");
        }
        other => panic!("expected opened, got {other:?}"),
    }
    // The instance's work is gone: a later start of it opens a new one.
    let again = tracker
        .resolve_spawn(None, Some("o-1"), Some(&orch), None)
        .kept()
        .unwrap();
    assert_ne!(again, a);
}

#[test]
fn an_end_before_its_start_is_written_after_it() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .resolve_spawn(None, Some("3"), None, None)
        .kept()
        .unwrap();
    tracker.attempt_ended(
        "9",
        AttemptEnd::of(
            &Termination::PaneCommandExited {
                code: 0,
                role: ChildRole::PaneCommand,
                reported_by: crate::inbox_display::TerminationSource::Deck,
            },
            false,
        ),
    );
    tracker.attempt_started(&work, "9", Some("3"), None);
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &work),
        ["opened", "attempt_started", "attempt_ended"]
    );
    assert_eq!(tracker.work_of_attempt("9"), None);
}

#[test]
fn a_delegation_is_a_child_of_its_orchestration_and_closes_on_its_report() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let orch = tracker
        .resolve_spawn(
            None,
            Some("o-2"),
            Some(&orchestration("orch-1", "coder")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("o-2"), Some("coder"));
    tracker.delegated(Some("orch-1"), "m-task-1", "coder", "o-2", "2");
    tracker.reported(&Reported {
        report_id: "r-1",
        about: ReportAbout::Task("m-task-1"),
        destination: Destination::Inbox,
    });
    let read = lines(&tracker, state.path());
    let task = read
        .iter()
        .find_map(|l| match &l.event {
            Event::Opened {
                work,
                launch: Launch::Delegation,
                parent,
                refs,
                ..
            } => {
                assert_eq!(parent.as_ref(), Some(&orch));
                assert_eq!(refs.task_id.as_deref(), Some("m-task-1"));
                assert_eq!(refs.role.as_deref(), Some("coder"));
                Some(work.clone())
            }
            _ => None,
        })
        .expect("the delegation is opened");
    assert_eq!(
        kinds_of(&read, &task),
        ["opened", "attempt_started", "reported", "closed"]
    );
    let reported = read.iter().find(|l| l.event.kind() == "reported").unwrap();
    match &reported.event {
        Event::Reported {
            report_id,
            task_id,
            destination,
            to,
            ..
        } => {
            assert_eq!(report_id, "r-1");
            assert_eq!(task_id.as_deref(), Some("m-task-1"));
            assert_eq!(*destination, Destination::Inbox);
            assert_eq!(to.as_ref(), Some(&orch));
        }
        other => panic!("expected reported, got {other:?}"),
    }
    // A second report of the same task finds nothing to write against.
    tracker.reported(&Reported {
        report_id: "r-2",
        about: ReportAbout::Task("m-task-1"),
        destination: Destination::Inbox,
    });
    assert_eq!(lines(&tracker, state.path()).len(), read.len());
}

#[test]
fn a_report_that_races_its_delegates_post_still_finds_the_tasks_work() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let orch = tracker
        .resolve_spawn(
            None,
            Some("o-2"),
            Some(&orchestration("orch-1", "coder")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("o-2"), Some("coder"));
    tracker.delegate_pending(Some("orch-1"), "m-task-1", "coder", "o-2", "2");
    // The worker's report is committed before the post path writes the
    // task's held events.
    tracker.reported(&Reported {
        report_id: "r-1",
        about: ReportAbout::Task("m-task-1"),
        destination: Destination::Inbox,
    });
    tracker.delegate_posted("m-task-1", &[]);
    let read = lines(&tracker, state.path());
    let task = read
        .iter()
        .find_map(|l| match &l.event {
            Event::Opened {
                work,
                launch: Launch::Delegation,
                ..
            } => Some(work.clone()),
            _ => None,
        })
        .expect("the delegation is opened");
    assert_eq!(
        kinds_of(&read, &task),
        ["opened", "attempt_started", "reported", "closed"]
    );
}

#[test]
fn a_refused_delegate_leaves_no_work() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let orch = tracker
        .resolve_spawn(
            None,
            Some("o-2"),
            Some(&orchestration("orch-1", "coder")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("o-2"), Some("coder"));
    let before = lines(&tracker, state.path()).len();
    tracker.delegate_pending(Some("orch-1"), "m-task-1", "coder", "o-2", "2");
    tracker.delegate_refused("m-task-1");
    // Neither a late post nor a report finds it.
    tracker.delegate_posted("m-task-1", &[]);
    tracker.reported(&Reported {
        report_id: "r-1",
        about: ReportAbout::Task("m-task-1"),
        destination: Destination::Inbox,
    });
    assert_eq!(lines(&tracker, state.path()).len(), before);
}

#[test]
fn a_dispatch_opens_with_its_unit_id_and_reports_to_its_dispatcher() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let dispatcher = tracker
        .resolve_spawn(None, Some("1"), None, None)
        .kept()
        .unwrap();
    tracker.attempt_started(&dispatcher, "1", Some("1"), None);
    let work = tracker
        .dispatch_minted(
            Some(&dispatcher),
            "agent/dispatch-fix-it",
            Path::new("/home/someone/code/x-dispatch-fix-it"),
        )
        .unwrap();
    // The unit's pane is an attempt of the dispatch's work, published before
    // the unit is registered: the held `opened` is not written by it.
    tracker.attempt_started(&work, "2", Some("dispatch-1"), None);
    assert_eq!(
        kinds_of(&lines(&tracker, state.path()), &work),
        ["attempt_started"]
    );
    tracker.unit_registered(&work, "u-1-1");
    tracker.flush_open(&work);
    tracker.reported(&Reported {
        report_id: "r-9",
        about: ReportAbout::Unit("u-1-1"),
        destination: Destination::Recovery,
    });
    tracker.unit_ended("u-1-1");
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &work),
        ["attempt_started", "opened", "reported", "closed"]
    );
    for line in read.iter().filter(|l| l.event.work() == &work) {
        match &line.event {
            Event::Opened {
                launch,
                parent,
                refs,
                workspace,
                ..
            } => {
                assert_eq!(*launch, Launch::Dispatch);
                assert_eq!(parent.as_ref(), Some(&dispatcher));
                assert_eq!(refs.unit_id.as_deref(), Some("u-1-1"));
                assert_eq!(refs.branch.as_deref(), Some("agent/dispatch-fix-it"));
                assert_eq!(
                    workspace.as_ref().unwrap().as_str(),
                    "~/code/x-dispatch-fix-it"
                );
            }
            Event::Reported {
                unit_id,
                to,
                destination,
                ..
            } => {
                assert_eq!(unit_id.as_deref(), Some("u-1-1"));
                assert_eq!(to.as_ref(), Some(&dispatcher));
                assert_eq!(*destination, Destination::Recovery);
            }
            Event::Closed { how, .. } => assert_eq!(*how, CloseHow::UnitEnded),
            _ => {}
        }
    }
}

#[test]
fn a_dispatch_that_started_nothing_leaves_no_work() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .dispatch_minted(None, "agent/dispatch-x", Path::new("/tmp/x"))
        .unwrap();
    tracker.forget(&work);
    tracker.flush_open(&work);
    assert!(lines(&tracker, state.path()).is_empty());
}

#[test]
fn a_plain_fire_closes_with_its_pane() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let fire = tracker
        .open(
            Launch::ScheduleFire,
            None,
            Refs {
                schedule: tracker.plain("nightly"),
                ..Refs::default()
            },
            None,
        )
        .unwrap();
    let served = tracker
        .resolve_spawn(Some(&fire), Some("sched-1"), None, None)
        .kept()
        .unwrap();
    assert_eq!(served, fire);
    tracker.attempt_started(&fire, "4", Some("sched-1"), None);
    // The fire spawned that pane: it closes with it, not now.
    tracker.fire_settled(&fire);
    assert_eq!(kinds_of(&lines(&tracker, state.path()), &fire).len(), 2);
    tracker.pane_closed("sched-1");
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &fire),
        ["opened", "attempt_started", "closed"]
    );
    assert!(matches!(
        read.last().unwrap().event,
        Event::Closed {
            how: CloseHow::FireDone,
            ..
        }
    ));
}

#[test]
fn a_ref_the_redaction_rule_refuses_is_left_out_not_the_event() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    // A pane id that reads as a path, and one with a control character.
    let a = tracker
        .resolve_spawn(None, Some("/etc/passwd"), None, None)
        .kept()
        .unwrap();
    tracker.attempt_started(&a, "1", Some("/etc/passwd"), Some("bad\nrole"));
    let read = lines(&tracker, state.path());
    assert_eq!(kinds_of(&read, &a), ["opened", "attempt_started"]);
    match &read[0].event {
        Event::Opened { refs, .. } => assert_eq!(refs.pane, None),
        other => panic!("expected opened, got {other:?}"),
    }
    match &read[1].event {
        Event::AttemptStarted { pane, role, .. } => {
            assert_eq!(pane, "");
            assert_eq!(*role, None);
        }
        other => panic!("expected attempt_started, got {other:?}"),
    }
}

/// The registry end to end: a spawned pane is handed its work in
/// `DOT_AGENT_DECK_WORK_ID` (a value the caller planted is stripped), the
/// work is pinned on its attested facts, and a respawn of the pane is handed
/// the same work as a new attempt, with the predecessor's end recorded.
#[tokio::test]
async fn a_spawn_is_handed_its_work_and_a_respawn_keeps_it() {
    let state = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let first = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), "wj-pane".to_string()),
                (ENV_WORK_ID.to_string(), "w-stale-planted".to_string()),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn");
    let work_env = |agent: &str| -> Vec<String> {
        registry
            .spawn_env_of_for_test(agent)
            .unwrap()
            .into_iter()
            .filter(|(k, _)| k == ENV_WORK_ID)
            .map(|(_, v)| v)
            .collect()
    };
    let work = registry
        .attest_facts(&first)
        .and_then(|facts| facts.work)
        .expect("the work is pinned on the generation");
    assert_eq!(work_env(&first), vec![work.to_string()]);

    let second = registry
        .respawn_agent_for_pane("wj-pane", "cat")
        .await
        .expect("respawn");
    assert_ne!(first, second);
    assert_eq!(
        work_env(&second),
        vec![work.to_string()],
        "a respawn replays its predecessor's env, and the work wins over it once"
    );
    assert_eq!(
        registry.attest_facts(&second).and_then(|facts| facts.work),
        Some(work.clone())
    );

    // The predecessor's end is written by its reader thread, which may run
    // after the successor's start is written: the end names the first
    // attempt, as replaced.
    let first_attempt = AttemptId::new("run1", first.parse().unwrap()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let read = lines(registry.work(), state.path());
        let ended = read.iter().find_map(|l| match &l.event {
            Event::AttemptEnded { attempt, how, .. } if l.event.work() == &work => {
                Some((attempt.clone(), *how))
            }
            _ => None,
        });
        if let Some(ended) = ended {
            assert_eq!(ended, (first_attempt, EndHow::Replaced));
            let mut kinds = kinds_of(&read, &work);
            kinds.sort_unstable();
            assert_eq!(
                kinds,
                [
                    "attempt_ended",
                    "attempt_started",
                    "attempt_started",
                    "opened"
                ]
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the first attempt's end was never written: {:?}",
            kinds_of(&read, &work)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    registry.shutdown_all();
}

/// With no journal installed (a registry the daemon did not set up), a spawn
/// gets no work id, and an inherited one is still stripped.
#[test]
fn a_registry_without_a_journal_hands_out_no_work() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let id = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), "wj-none".to_string()),
                (ENV_WORK_ID.to_string(), "w-stale-planted".to_string()),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn");
    let env = registry.spawn_env_of_for_test(&id).unwrap();
    assert!(!env.iter().any(|(k, _)| k == ENV_WORK_ID), "{env:?}");
    assert_eq!(registry.attest_facts(&id).and_then(|f| f.work), None);
    registry.shutdown_all();
}

// ---------------------------------------------------------------------------
// Bounded bookkeeping (finding A)
// ---------------------------------------------------------------------------

fn worker_end() -> AttemptEnd {
    AttemptEnd::of(&Termination::Closed, false)
}

#[test]
fn a_spawn_that_publishes_nothing_forgets_what_it_minted() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let baseline = tracker.book_sizes();
    for n in 0..100 {
        // A pane spawn refused or failed before its first attempt.
        let guard = tracker.resolve_spawn(None, Some(&format!("p-{n}")), None, None);
        assert!(guard.work().is_some());
        drop(guard);
        // An orchestration's first role that never started.
        let guard = tracker.resolve_spawn(
            None,
            Some("o-1"),
            Some(&orchestration(&format!("orch-{n}"), "orchestrator")),
            None,
        );
        drop(guard);
    }
    assert_eq!(tracker.book_sizes(), baseline);
    assert!(lines(&tracker, state.path()).is_empty());

    // Two roles of one orchestration on their way: the first fails, the
    // second publishes. The shared work survives the first's failure.
    let orch = orchestration("orch-shared", "orchestrator");
    let coder = orchestration("orch-shared", "coder");
    let first = tracker.resolve_spawn(None, Some("o-1"), Some(&orch), None);
    let second = tracker.resolve_spawn(None, Some("o-2"), Some(&coder), None);
    let work = second.work().cloned().unwrap();
    assert_eq!(first.work(), Some(&work));
    drop(first);
    assert_eq!(
        tracker.orchestration_work("orch-shared"),
        Some(work.clone())
    );
    tracker.attempt_started(&work, "2", Some("o-2"), Some("coder"));
    drop(second);
    assert_eq!(
        tracker.orchestration_work("orch-shared"),
        Some(work.clone())
    );
    assert_eq!(
        kinds_of(&lines(&tracker, state.path()), &work),
        ["opened", "attempt_started"]
    );
    // A third role failing after the work is published removes nothing.
    drop(tracker.resolve_spawn(None, Some("o-3"), Some(&orch), None));
    assert_eq!(tracker.orchestration_work("orch-shared"), Some(work));
    assert_eq!(tracker.book_sizes().spawning, 0);
}

/// The registry end to end: refusing a duplicate pane over and over, and
/// failing a spawn after its fork, each leave the book where it was.
#[tokio::test]
async fn refused_and_failed_spawns_leave_the_book_at_its_baseline() {
    let state = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let spawn = |pane: &str| {
        registry.spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            ..SpawnOptions::default()
        })
    };
    spawn("wj-dup").expect("the first spawn");
    let baseline = registry.work().book_sizes();
    for _ in 0..25 {
        assert!(spawn("wj-dup").is_err(), "a duplicate pane was admitted");
    }
    assert_eq!(registry.work().book_sizes(), baseline);
    for n in 0..5 {
        registry.inject_fault_for_test(
            crate::agent_pty::FAULT_SPAWN_BEFORE_PUBLISH,
            crate::agent_pty::TestFault::Panic("injected"),
        );
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            spawn(&format!("wj-fail-{n}"))
        }));
        assert!(failed.is_err(), "the injected fault did not fire");
    }
    assert_eq!(registry.work().book_sizes(), baseline);
    registry.shutdown_all();
}

#[test]
fn superseded_abandoned_and_torn_down_delegations_are_retired() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let orch = tracker
        .resolve_spawn(
            None,
            Some("o-2"),
            Some(&orchestration("orch-1", "coder")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("o-2"), Some("coder"));
    let baseline = tracker.book_sizes();
    // Each new task to the pane supersedes the one before it.
    for n in 0..200 {
        tracker.delegated(Some("orch-1"), &format!("m-{n}"), "coder", "o-2", "2");
        assert_eq!(tracker.book_sizes().tasks, 1);
    }
    assert!(tracker.book_sizes().retired <= MAX_RETIRED_TASKS);
    // A report already on its way for a superseded task still finds it.
    tracker.reported(&Reported {
        report_id: "r-198",
        about: ReportAbout::Task("m-198"),
        destination: Destination::Inbox,
    });
    let read = lines(&tracker, state.path());
    assert!(read.iter().any(|l| matches!(
        &l.event,
        Event::Reported { task_id: Some(t), .. } if t == "m-198"
    )));
    // The worker generation the last task was bound to ends.
    tracker.attempt_ended("2", worker_end());
    assert_eq!(tracker.book_sizes().tasks, 0);

    // A new generation's tasks go when the orchestration ends.
    tracker.attempt_started(&orch, "3", Some("o-2"), Some("coder"));
    tracker.delegated(Some("orch-1"), "m-late", "coder", "o-2", "3");
    tracker.delegate_pending(Some("orch-1"), "m-never-posted", "coder", "o-3", "4");
    tracker.attempt_ended("3", worker_end());
    tracker.orchestration_ended("orch-1");
    let sizes = tracker.book_sizes();
    assert_eq!(sizes.tasks, 0);
    assert!(sizes.retired <= MAX_RETIRED_TASKS);
    assert!(sizes.settled <= MAX_SETTLED_TASKS);
    assert_eq!(
        BookSizes {
            retired: 0,
            settled: 0,
            orchestrations: 0,
            panes: 0,
            ..sizes
        },
        BookSizes {
            retired: 0,
            settled: 0,
            orchestrations: 0,
            attempts: 0,
            panes: 0,
            ..baseline
        }
    );
}

/// The delegation work opened for task `task_id`, if its `opened` is written.
fn task_work(read: &[Line], task_id: &str) -> Option<WorkId> {
    read.iter().find_map(|l| match &l.event {
        Event::Opened {
            work,
            launch: Launch::Delegation,
            refs,
            ..
        } if refs.task_id.as_deref() == Some(task_id) => Some(work.clone()),
        _ => None,
    })
}

fn delegation_fixture(state: &Path) -> WorkTracker {
    let tracker = tracker(state);
    let orch = tracker
        .resolve_spawn(
            None,
            Some("o-2"),
            Some(&orchestration("orch-1", "coder")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("o-2"), Some("coder"));
    tracker
}

/// Reviewer finding 3: two delegations to one pane are both pending (each
/// registered, neither posted). A's post completes first, superseding nothing
/// the hub had (B was not posted yet), so B must stay live: its post later
/// writes its `opened` and `attempt_started`, and it supersedes A by id.
#[test]
fn a_pending_newer_delegation_survives_an_older_ones_post() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = delegation_fixture(state.path());
    tracker.delegate_pending(Some("orch-1"), "m-a", "coder", "o-2", "2");
    tracker.delegate_pending(Some("orch-1"), "m-b", "coder", "o-2", "2");
    tracker.delegate_posted("m-a", &[]);
    assert_eq!(tracker.book_sizes().tasks, 2, "B was retired by A's post");
    assert_eq!(tracker.book_sizes().retired, 0);
    tracker.delegate_posted("m-b", &["m-a".to_string()]);
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.tasks, sizes.retired), (1, 1));
    let read = lines(&tracker, state.path());
    let b = task_work(&read, "m-b").expect("B's opened is written");
    assert_eq!(kinds_of(&read, &b), ["opened", "attempt_started"]);
    // B's report still finds B; a late report of the superseded A finds A.
    tracker.reported(&Reported {
        report_id: "r-b",
        about: ReportAbout::Task("m-b"),
        destination: Destination::Inbox,
    });
    tracker.reported(&Reported {
        report_id: "r-a",
        about: ReportAbout::Task("m-a"),
        destination: Destination::Inbox,
    });
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &b),
        ["opened", "attempt_started", "reported", "closed"]
    );
    let a = task_work(&read, "m-a").expect("A's opened is written");
    assert_eq!(
        kinds_of(&read, &a),
        ["opened", "attempt_started", "reported", "closed"]
    );
    assert_eq!(tracker.book_sizes().tasks, 0);
}

/// The posts complete out of order: B (registered second) is posted first,
/// then A, whose post the hub saw after B's and so superseded B. Exactly B is
/// retired, A stays live, and both have their `opened` and
/// `attempt_started`.
#[test]
fn out_of_order_post_completions_retire_exactly_the_superseded_task() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = delegation_fixture(state.path());
    tracker.delegate_pending(Some("orch-1"), "m-a", "coder", "o-2", "2");
    tracker.delegate_pending(Some("orch-1"), "m-b", "coder", "o-2", "2");
    tracker.delegate_posted("m-b", &[]);
    assert_eq!(tracker.book_sizes().tasks, 2);
    tracker.delegate_posted("m-a", &["m-b".to_string()]);
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.tasks, sizes.retired), (1, 1));
    let read = lines(&tracker, state.path());
    for task in ["m-a", "m-b"] {
        let work = task_work(&read, task).unwrap_or_else(|| panic!("{task} opened"));
        assert_eq!(
            kinds_of(&read, &work),
            ["opened", "attempt_started"],
            "{task}"
        );
    }
    // A, the live one, is the one A's report closes from the live table.
    tracker.reported(&Reported {
        report_id: "r-a",
        about: ReportAbout::Task("m-a"),
        destination: Destination::Inbox,
    });
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.tasks, sizes.retired), (0, 1));
}

/// A task's report wins against its own post, and that post superseded an
/// older task: the older one is still retired by id, and a superseded id the
/// tracker never knew (a task posted before the journal) is ignored.
#[test]
fn a_post_whose_report_won_still_retires_what_it_superseded() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = delegation_fixture(state.path());
    tracker.delegated(Some("orch-1"), "m-old", "coder", "o-2", "2");
    tracker.delegate_pending(Some("orch-1"), "m-new", "coder", "o-2", "2");
    tracker.reported(&Reported {
        report_id: "r-new",
        about: ReportAbout::Task("m-new"),
        destination: Destination::Inbox,
    });
    tracker.delegate_posted("m-new", &["m-old".to_string(), "m-unknown".to_string()]);
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.tasks, sizes.retired), (0, 1));
    let read = lines(&tracker, state.path());
    let new = task_work(&read, "m-new").unwrap();
    assert_eq!(
        kinds_of(&read, &new),
        ["opened", "attempt_started", "reported", "closed"]
    );
}

/// Reviewer suggestion 1 / auditor S3: the hub accepts A, then B (superseding
/// A), and B's callback runs before A's, so A is already in the retired ring
/// when its own callback arrives. The inbox accepted A, so A's `opened` and
/// `attempt_started` are written anyway, with no report needed to rescue them.
#[test]
fn a_post_whose_task_a_later_post_retired_first_still_writes_its_events() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = delegation_fixture(state.path());
    tracker.delegate_pending(Some("orch-1"), "m-a", "coder", "o-2", "2");
    tracker.delegate_pending(Some("orch-1"), "m-b", "coder", "o-2", "2");
    tracker.delegate_posted("m-b", &["m-a".to_string()]);
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.tasks, sizes.retired), (1, 1));
    tracker.delegate_posted("m-a", &[]);
    let read = lines(&tracker, state.path());
    for task in ["m-a", "m-b"] {
        let work = task_work(&read, task).unwrap_or_else(|| panic!("{task} opened"));
        assert_eq!(
            kinds_of(&read, &work),
            ["opened", "attempt_started"],
            "{task}"
        );
    }
    // A stays retired (B superseded it); a late report of A still finds it.
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.tasks, sizes.retired), (1, 1));
}

/// Auditor S3: A's worker generation ends between the hub accepting A and A's
/// callback, which retires A with its events still held. The callback writes
/// them: A has `opened` and an `attempt_started` naming the generation, whose
/// one `attempt_ended` (on the orchestration's work, keyed by attempt id) ends
/// A's attempt too. Many settled retirements after it do not evict A from the
/// ring before its callback comes.
#[test]
fn a_post_whose_worker_ended_before_its_callback_still_writes_its_events() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let orch = tracker
        .resolve_spawn(
            None,
            Some("o-2"),
            Some(&orchestration("orch-1", "coder")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("o-2"), Some("coder"));
    tracker.delegate_pending(Some("orch-1"), "m-a", "coder", "o-2", "2");
    tracker.attempt_ended("2", worker_end());
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.tasks, sizes.retired), (0, 1));
    // A respawned generation's tasks, each superseding the one before it,
    // overflow the ring with settled entries.
    tracker.attempt_started(&orch, "3", Some("o-2"), Some("coder"));
    for n in 0..(2 * MAX_RETIRED_TASKS) {
        tracker.delegated(Some("orch-1"), &format!("m-{n}"), "coder", "o-2", "3");
    }
    assert!(tracker.book_sizes().retired <= MAX_RETIRED_TASKS);
    tracker.delegate_posted("m-a", &[]);
    let read = lines(&tracker, state.path());
    let a = task_work(&read, "m-a").expect("A's opened is written");
    assert_eq!(kinds_of(&read, &a), ["opened", "attempt_started"]);
    let started = read
        .iter()
        .find_map(|l| match &l.event {
            Event::AttemptStarted { work, attempt, .. } if *work == a => Some(attempt.clone()),
            _ => None,
        })
        .unwrap();
    let ends: Vec<&WorkId> = read
        .iter()
        .filter_map(|l| match &l.event {
            Event::AttemptEnded { work, attempt, .. } if *attempt == started => Some(work),
            _ => None,
        })
        .collect();
    assert_eq!(
        ends,
        [&orch],
        "the generation's one end, on its role's work"
    );
    // Settled, A now ages out of the ring like any other retired task.
    let sizes = tracker.book_sizes();
    assert_eq!(sizes.tasks, 1);
    assert!(sizes.retired <= MAX_RETIRED_TASKS);
}

/// A refused post forgets its task even when it was retired while the post
/// was in flight, and writes nothing of it.
#[test]
fn a_refused_post_whose_task_was_retired_leaves_nothing() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = delegation_fixture(state.path());
    let baseline = tracker.book_sizes();
    tracker.delegate_pending(Some("orch-1"), "m-a", "coder", "o-2", "2");
    tracker.orchestration_ended("orch-1");
    assert_eq!(tracker.book_sizes().retired, 1);
    tracker.delegate_refused("m-a");
    assert_eq!(tracker.book_sizes().retired, 0);
    assert!(task_work(&lines(&tracker, state.path()), "m-a").is_none());
    assert_eq!(tracker.book_sizes().tasks, baseline.tasks);
}

#[test]
fn a_fire_that_reuses_a_pane_or_opens_an_orchestration_closes_at_once() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let baseline = tracker.book_sizes();
    let mut fires = Vec::new();
    for _ in 0..50 {
        // A reuse fire: re-posted into a pane an earlier fire opened, so no
        // attempt of its own.
        let fire = tracker
            .open(Launch::ScheduleFire, None, Refs::default(), None)
            .unwrap();
        tracker.fire_settled(&fire);
        fires.push(fire);
    }
    assert_eq!(tracker.book_sizes(), baseline);
    let read = lines(&tracker, state.path());
    for fire in &fires {
        assert_eq!(kinds_of(&read, fire), ["opened", "closed"]);
    }
    assert!(read.iter().all(|l| !matches!(
        l.event,
        Event::Closed { ref how, .. } if *how != CloseHow::FireDone
    )));

    // A fire that opened an orchestration: its role panes serve the
    // orchestration's work (its child), so the fire itself is done.
    let fire = tracker
        .open(Launch::ScheduleFire, None, Refs::default(), None)
        .unwrap();
    let orch = tracker
        .resolve_spawn(
            Some(&fire),
            Some("o-1"),
            Some(&orchestration("orch-9", "orchestrator")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "1", Some("o-1"), Some("orchestrator"));
    tracker.fire_settled(&fire);
    assert_eq!(
        kinds_of(&lines(&tracker, state.path()), &fire),
        ["opened", "closed"]
    );
    assert_eq!(tracker.book_sizes().pane_works, baseline.pane_works);
}

// ---------------------------------------------------------------------------
// A stalled journal writer (finding B)
// ---------------------------------------------------------------------------

#[test]
fn a_blocked_writer_never_holds_up_spawn_report_or_bookkeeping() {
    let state = crate::test_temp::tempdir().unwrap();
    let gate = crate::work_journal::WriterGate::closed();
    let mut options = JournalOptions {
        home: PathBuf::from("/home/someone"),
        queue_capacity: 4,
        ..JournalOptions::default()
    };
    options.hooks.gate = Some(Arc::clone(&gate));
    let tracker = WorkTracker::default();
    tracker.install(
        WorkJournal::try_open_with(state.path(), "run1", &DeckId::generate(), options).unwrap(),
    );
    // Every lifecycle call returns while the writer cannot write a byte: a
    // hang here is the failure.
    for n in 0..100 {
        let pane = format!("p-{n}");
        let agent = format!("{}", 1000 + n);
        let work = tracker
            .resolve_spawn(None, Some(&pane), None, None)
            .kept()
            .unwrap();
        tracker.attempt_started(&work, &agent, Some(&pane), None);
        tracker.delegated(None, &format!("m-{n}"), "coder", &pane, &agent);
        tracker.reported(&Reported {
            report_id: "r-1",
            about: ReportAbout::Task(&format!("m-{n}")),
            destination: Destination::Inbox,
        });
        let unit = tracker
            .dispatch_minted(Some(&work), "agent/x", Path::new("/tmp/x"))
            .unwrap();
        tracker.unit_registered(&unit, &format!("u-{n}"));
        tracker.flush_open(&unit);
        tracker.unit_ended(&format!("u-{n}"));
        tracker.attempt_ended(&agent, worker_end());
        tracker.pane_closed(&pane);
    }
    let journal = tracker.journal().unwrap();
    assert!(journal.dropped_events() > 0, "the queue never filled");
    // The book is not held by the stalled writer either.
    assert_eq!(tracker.book_sizes().tasks, 0);
    gate.open();
    journal.flush();
    assert!(!lines(&tracker, state.path()).is_empty());
}

/// The registry end to end with its journal's writer stalled: a spawn, the
/// generation's facts and its record are all available.
#[tokio::test]
async fn a_blocked_writer_never_holds_up_the_registry() {
    let state = crate::test_temp::tempdir().unwrap();
    let gate = crate::work_journal::WriterGate::closed();
    let mut options = JournalOptions {
        home: PathBuf::from("/home/someone"),
        queue_capacity: 1,
        ..JournalOptions::default()
    };
    options.hooks.gate = Some(Arc::clone(&gate));
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(
        WorkJournal::try_open_with(state.path(), "run1", &DeckId::generate(), options).unwrap(),
    );
    let mut ids = Vec::new();
    for n in 0..5 {
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), format!("wj-gate-{n}"))],
                ..SpawnOptions::default()
            })
            .expect("spawn");
        assert!(registry.attest_facts(&id).and_then(|f| f.work).is_some());
        ids.push(id);
    }
    assert_eq!(registry.live_pane_agents().len(), 5);
    registry.shutdown_all();
    gate.open();
}

/// Greptile P2 on PR #2248: a stop that closes a freshly published pane
/// before the spawn has returned must still close the pane's work. The spawn
/// is held just after publication, the pane is closed there (what a
/// concurrent `StopAgent` does once it can find the generation), and the
/// pane's work must read closed — not opened with nothing left to close it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pane_closed_while_its_spawn_returns_closes_its_work() {
    let state = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let (reached, release) =
        registry.pause_at_sync_for_test(crate::agent_pty::PAUSE_SPAWN_AFTER_PUBLISH);
    let spawner = Arc::clone(&registry);
    let spawn = tokio::task::spawn_blocking(move || {
        spawner.spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "wj-race".to_string())],
            ..SpawnOptions::default()
        })
    });
    reached
        .recv_timeout(Duration::from_secs(20))
        .expect("the spawn reached its post-publication window");
    // A stop's close step, run while the spawn has not returned yet.
    registry.work().pane_closed("wj-race");
    release.send(()).unwrap();
    let agent = spawn.await.unwrap().expect("spawn");
    let work = registry
        .attest_facts(&agent)
        .and_then(|facts| facts.work)
        .expect("the work is pinned on the generation");
    let read = lines(registry.work(), state.path());
    let kinds = kinds_of(&read, &work);
    assert!(
        kinds.contains(&"closed"),
        "a pane closed during its spawn left its work open: {kinds:?}"
    );
    assert_eq!(kinds[..2], ["opened", "attempt_started"]);
    registry.shutdown_all();
}

/// Qodo finding 3 on PR #2248: a report of a superseded task that has been
/// evicted from the retired ring (many newer tasks went to the same pane
/// since) is still written on that task's work and closes it, and the
/// task-to-work association stays bounded.
#[test]
fn a_late_report_of_a_task_evicted_from_the_retired_ring_still_finds_its_work() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = delegation_fixture(state.path());
    for n in 0..(MAX_RETIRED_TASKS + 100) {
        tracker.delegated(Some("orch-1"), &format!("m-{n}"), "coder", "o-2", "2");
    }
    let sizes = tracker.book_sizes();
    assert!(sizes.retired <= MAX_RETIRED_TASKS);
    assert!(sizes.settled > 0, "nothing was evicted from the ring");
    tracker.reported(&Reported {
        report_id: "r-5",
        about: ReportAbout::Task("m-5"),
        destination: Destination::Inbox,
    });
    let read = lines(&tracker, state.path());
    let work = task_work(&read, "m-5").expect("m-5's opened is written");
    assert_eq!(
        kinds_of(&read, &work),
        ["opened", "attempt_started", "reported", "closed"]
    );
    assert_eq!(tracker.book_sizes().settled, sizes.settled - 1);

    // The association is bounded: the oldest settled tasks go first.
    for n in 0..(MAX_SETTLED_TASKS + 10) {
        tracker.delegated(Some("orch-1"), &format!("n-{n}"), "coder", "o-2", "2");
    }
    assert!(tracker.book_sizes().settled <= MAX_SETTLED_TASKS);
}

/// Greptile P2 on PR #2248: a dispatched orchestration whose first role came
/// up (writing the orchestration's `opened` with the dispatch as parent) and
/// whose later role failed. The rollback closes the orchestration, and the
/// failed dispatch keeps its own `opened` — so the child's parent can be
/// found — and closes. A dispatch that started nothing still leaves nothing.
#[test]
fn a_failed_dispatch_whose_orchestration_started_keeps_and_closes_its_work() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .dispatch_minted(None, "agent/dispatch-team", Path::new("/tmp/team"))
        .unwrap();
    let orch = tracker
        .resolve_spawn(
            Some(&work),
            Some("o-1"),
            Some(&orchestration("orch-d", "orchestrator")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "1", Some("o-1"), Some("orchestrator"));
    // The second role fails: the rollback stops the first and the
    // orchestration ends, before any unit was registered.
    tracker.attempt_ended("1", worker_end());
    tracker.orchestration_ended("orch-d");
    tracker.launch_failed(&work, Some(CloseHow::UnitEnded));
    let read = lines(&tracker, state.path());
    assert_eq!(kinds_of(&read, &work), ["opened", "closed"]);
    let parent = read.iter().find_map(|l| match &l.event {
        Event::Opened {
            work: w, parent, ..
        } if w == &orch => Some(parent.clone()),
        _ => None,
    });
    assert_eq!(parent, Some(Some(work.clone())));
    assert!(read.iter().any(|l| matches!(
        &l.event,
        Event::Closed { work: w, how: CloseHow::UnitEnded } if w == &work
    )));
    assert_eq!(tracker.book_sizes().launches, 0);
    assert_eq!(tracker.book_sizes().pending_units, 0);

    // Nothing started: nothing written.
    let quiet = tracker
        .dispatch_minted(None, "agent/dispatch-quiet", Path::new("/tmp/quiet"))
        .unwrap();
    tracker.launch_failed(&quiet, Some(CloseHow::UnitEnded));
    assert!(kinds_of(&lines(&tracker, state.path()), &quiet).is_empty());
}

fn issue_launch(tracker: &WorkTracker, fire: Option<&WorkId>) -> WorkId {
    tracker
        .hold_launch(
            Launch::IssueDispatch,
            fire,
            Refs {
                issue: Some(7),
                ..Refs::default()
            },
            Path::new("/home/someone/code/x-issue-7"),
        )
        .unwrap()
}

/// Qodo finding 6 on PR #2248: a scheduled issue dispatch's work is held
/// until its spawn settles. A spawn that fails having started nothing leaves
/// no work; a single-agent one closes with its pane (`pane_closed`); an
/// orchestration-shaped one closes when its orchestration ends
/// (`orchestration_ended`); and the book returns to where it was.
#[test]
fn a_scheduled_issue_dispatch_closes_with_what_it_started_or_leaves_nothing() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let fire = tracker.open(Launch::ScheduleFire, None, Refs::default(), None);
    let baseline = tracker.book_sizes();

    // The spawn fails before anything starts.
    let failed = issue_launch(&tracker, fire.as_ref());
    tracker.launch_failed(&failed, None);
    assert!(kinds_of(&lines(&tracker, state.path()), &failed).is_empty());
    assert_eq!(tracker.book_sizes(), baseline);

    // A single agent: its pane is an attempt of the issue's work.
    let single = issue_launch(&tracker, fire.as_ref());
    let served = tracker
        .resolve_spawn(Some(&single), Some("i-1"), None, None)
        .kept()
        .unwrap();
    assert_eq!(served, single);
    tracker.attempt_started(&single, "1", Some("i-1"), None);
    tracker.flush_open(&single);
    tracker.issue_launched(&single);
    assert_eq!(
        kinds_of(&lines(&tracker, state.path()), &single),
        ["opened", "attempt_started"],
        "open while its pane is"
    );
    tracker.attempt_ended("1", worker_end());
    tracker.pane_closed("i-1");
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &single),
        ["opened", "attempt_started", "attempt_ended", "closed"]
    );
    assert!(read.iter().any(|l| matches!(
        &l.event,
        Event::Closed { work, how: CloseHow::PaneClosed } if work == &single
    )));

    // An orchestration: its work is the issue's child, and the issue's work
    // closes when it ends.
    let team = issue_launch(&tracker, fire.as_ref());
    let orch = tracker
        .resolve_spawn(
            Some(&team),
            Some("io-1"),
            Some(&orchestration("orch-issue", "orchestrator")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("io-1"), Some("orchestrator"));
    tracker.flush_open(&team);
    tracker.issue_launched(&team);
    assert_eq!(kinds_of(&lines(&tracker, state.path()), &team), ["opened"]);
    tracker.attempt_ended("2", worker_end());
    tracker.orchestration_ended("orch-issue");
    let read = lines(&tracker, state.path());
    assert_eq!(kinds_of(&read, &team), ["opened", "closed"]);
    assert!(read.iter().any(|l| matches!(
        &l.event,
        Event::Closed { work, how: CloseHow::OrchestrationEnded } if work == &team
    )));
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.launches, sizes.close_with_child), (0, 0));

    // An orchestration-shaped spawn whose later role failed: the issue's
    // work keeps its `opened` and closes with the orchestration it started.
    let broken = issue_launch(&tracker, fire.as_ref());
    let orch = tracker
        .resolve_spawn(
            Some(&broken),
            Some("ib-1"),
            Some(&orchestration("orch-broken", "orchestrator")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "3", Some("ib-1"), Some("orchestrator"));
    tracker.attempt_ended("3", worker_end());
    tracker.orchestration_ended("orch-broken");
    tracker.launch_failed(&broken, None);
    let read = lines(&tracker, state.path());
    assert_eq!(kinds_of(&read, &broken), ["opened", "closed"]);
    assert_eq!(tracker.book_sizes().launches, 0);
}
