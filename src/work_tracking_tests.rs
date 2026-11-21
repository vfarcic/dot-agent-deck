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
    tracker.launched_without_unit(&single);
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
    tracker.launched_without_unit(&team);
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

/// PR #2431 round 5: a launch this daemon's shutdown cut short is kept open
/// once something of it was written — a scheduled issue dispatch's pane, or
/// a dispatch's orchestration, whose held `opened` is written now — so the
/// next daemon lists it as interrupted work. One that wrote nothing is
/// forgotten, as a failed launch is, and answers that nothing was kept.
#[test]
fn a_launch_the_shutdown_cut_short_stays_open_once_something_of_it_was_written() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let fire = tracker.open(Launch::ScheduleFire, None, Refs::default(), None);

    // A scheduled issue dispatch whose single agent came up.
    let single = issue_launch(&tracker, fire.as_ref());
    tracker
        .resolve_spawn(Some(&single), Some("i-1"), None, None)
        .kept()
        .unwrap();
    tracker.attempt_started(&single, "1", Some("i-1"), None);
    assert!(tracker.launch_interrupted(&single));
    tracker.pane_closed("i-1");
    assert_eq!(
        kinds_of(&lines(&tracker, state.path()), &single),
        ["opened", "attempt_started"],
        "no `closed`, even when its pane goes"
    );

    // A dispatch whose orchestration's first role came up before its unit
    // was registered: its own `opened`, still held, is written.
    let team = tracker
        .dispatch_minted(None, "agent/dispatch-team", Path::new("/tmp/team"))
        .unwrap();
    let orch = tracker
        .resolve_spawn(
            Some(&team),
            Some("o-1"),
            Some(&orchestration("orch-cut", "orchestrator")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("o-1"), Some("orchestrator"));
    assert!(tracker.launch_interrupted(&team));
    let read = lines(&tracker, state.path());
    assert_eq!(kinds_of(&read, &team), ["opened"]);
    assert_eq!(kinds_of(&read, &orch), ["opened", "attempt_started"]);

    // Nothing started: nothing written, nothing kept.
    let quiet = tracker
        .dispatch_minted(None, "agent/dispatch-quiet", Path::new("/tmp/quiet"))
        .unwrap();
    assert!(!tracker.launch_interrupted(&quiet));
    assert!(kinds_of(&lines(&tracker, state.path()), &quiet).is_empty());
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.launches, sizes.pending_units), (0, 0));
}

/// Qodo finding 1 (second round) on PR #2248: a `dispatch` with no caller
/// spawns without registering a unit, so no unit's end can close its work.
/// It closes with what it spawned instead: a single agent's pane, or the
/// orchestration it started, and at once when that is already gone.
#[test]
fn a_dispatch_with_no_caller_closes_with_what_it_spawned() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());

    // A single agent: closes `pane_closed` when its pane closes.
    let single = tracker
        .dispatch_minted(None, "agent/dispatch-solo", Path::new("/tmp/solo"))
        .unwrap();
    tracker.attempt_started(&single, "1", Some("d-1"), None);
    tracker.launched_without_unit(&single);
    assert_eq!(
        kinds_of(&lines(&tracker, state.path()), &single),
        ["attempt_started", "opened"],
        "open while its pane is"
    );
    tracker.attempt_ended("1", worker_end());
    tracker.pane_closed("d-1");
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &single),
        ["attempt_started", "opened", "attempt_ended", "closed"]
    );
    assert!(read.iter().any(|l| matches!(
        &l.event,
        Event::Closed { work, how: CloseHow::PaneClosed } if work == &single
    )));

    // An orchestration: closes `orchestration_ended` when it ends.
    let team = tracker
        .dispatch_minted(None, "agent/dispatch-team", Path::new("/tmp/team"))
        .unwrap();
    let orch = tracker
        .resolve_spawn(
            Some(&team),
            Some("do-1"),
            Some(&orchestration("orch-nocaller", "orchestrator")),
            None,
        )
        .kept()
        .unwrap();
    tracker.attempt_started(&orch, "2", Some("do-1"), Some("orchestrator"));
    tracker.launched_without_unit(&team);
    assert_eq!(kinds_of(&lines(&tracker, state.path()), &team), ["opened"]);
    tracker.attempt_ended("2", worker_end());
    tracker.orchestration_ended("orch-nocaller");
    let read = lines(&tracker, state.path());
    assert_eq!(kinds_of(&read, &team), ["opened", "closed"]);
    assert!(read.iter().any(|l| matches!(
        &l.event,
        Event::Closed { work, how: CloseHow::OrchestrationEnded } if work == &team
    )));

    // Its pane already closed by the time the spawn returned: closes at once.
    let gone = tracker
        .dispatch_minted(None, "agent/dispatch-gone", Path::new("/tmp/gone"))
        .unwrap();
    tracker.attempt_started(&gone, "3", Some("d-3"), None);
    tracker.attempt_ended("3", worker_end());
    tracker.pane_closed("d-3");
    tracker.launched_without_unit(&gone);
    let read = lines(&tracker, state.path());
    assert_eq!(
        kinds_of(&read, &gone),
        ["attempt_started", "attempt_ended", "opened", "closed"]
    );
    assert!(read.iter().any(|l| matches!(
        &l.event,
        Event::Closed { work, how: CloseHow::PaneClosed } if work == &gone
    )));
    let sizes = tracker.book_sizes();
    assert_eq!(
        (sizes.launches, sizes.close_with_child, sizes.pending_units),
        (0, 0, 0)
    );
}

/// The `attempt_session` lines of `work`, in order, as (attempt, session).
fn sessions_of(read: &[Line], work: &WorkId) -> Vec<(String, String)> {
    read.iter()
        .filter_map(|l| match &l.event {
            Event::AttemptSession {
                work: w,
                attempt,
                session,
                ..
            } if w == work => Some((attempt.to_string(), session.clone())),
            _ => None,
        })
        .collect()
}

/// PRD #1790: an attempt's session id is written when it first appears and
/// each time it changes, never again while it is unchanged; an id reported by
/// a generation whose spawn is reserved and not yet published is held and
/// written, in order, right after its `attempt_started`, while one for a
/// generation this run is not starting is not; an attempt that has ended
/// writes nothing, and the book is retired with the attempt.
#[test]
fn a_session_is_written_when_it_appears_and_when_it_changes_only() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .resolve_spawn(None, Some("p"), None, None)
        .kept()
        .unwrap();
    let claude = AgentType::ClaudeCode;
    // Not reserved and not started: nothing to hold it for.
    tracker.session_seen("6", Some("p"), "s-stray", &claude, false);
    // Reserved, before its start: held, in order, deduplicated.
    for session in ["s-0", "s-0", "s-early"] {
        tracker.session_seen("7", Some("p"), session, &claude, true);
    }
    assert_eq!(tracker.book_sizes().early_sessions, 1);
    tracker.attempt_started(&work, "7", Some("p"), None);
    assert_eq!(
        tracker.book_sizes().early_sessions,
        0,
        "drained at its start"
    );
    for session in ["s-early", "s-1", "s-1", "s-2", "s-2", "s-1"] {
        tracker.session_seen("7", Some("p"), session, &claude, false);
    }
    // A path-like id is refused by the redaction rule, not written.
    tracker.session_seen("7", Some("p"), "/home/someone/x", &claude, false);
    assert_eq!(tracker.book_sizes().sessions, 1);
    tracker.attempt_ended("7", worker_end());
    assert_eq!(tracker.book_sizes().sessions, 0, "retired with its attempt");
    // After its end, nothing more is written for it.
    tracker.session_seen("7", Some("p"), "s-3", &claude, false);
    let read = lines(&tracker, state.path());
    let s = |id: &str| ("run1/7".to_string(), id.to_string());
    assert_eq!(
        sessions_of(&read, &work),
        [s("s-0"), s("s-early"), s("s-1"), s("s-2"), s("s-1")]
    );
    // The held ids follow the attempt's start.
    let kinds = kinds_of(&read, &work);
    let started = kinds.iter().position(|k| *k == "attempt_started").unwrap();
    let first_session = kinds.iter().position(|k| *k == "attempt_session").unwrap();
    assert!(started < first_session, "{kinds:?}");
    assert!(read.iter().all(|l| match &l.event {
        Event::AttemptSession { agent, .. } => *agent == AgentType::ClaudeCode,
        _ => true,
    }));
}

/// PRD #1790 (R1): the ids held for a reserved generation go when its spawn
/// is abandoned, and an end that arrived before its start still follows the
/// held ids.
#[test]
fn held_sessions_go_with_an_abandoned_spawn_and_precede_an_early_end() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .resolve_spawn(None, Some("p"), None, None)
        .kept()
        .unwrap();
    let claude = AgentType::ClaudeCode;
    tracker.session_seen("4", Some("p"), "s-abandoned", &claude, true);
    tracker.spawn_abandoned("4");
    assert_eq!(tracker.book_sizes().early_sessions, 0);
    tracker.session_seen("5", Some("p"), "s-five", &claude, true);
    tracker.attempt_ended("5", worker_end());
    tracker.attempt_started(&work, "5", Some("p"), None);
    let read = lines(&tracker, state.path());
    assert_eq!(
        sessions_of(&read, &work),
        [("run1/5".to_string(), "s-five".to_string())]
    );
    let kinds = kinds_of(&read, &work);
    assert_eq!(
        &kinds[kinds.len() - 3..],
        ["attempt_started", "attempt_session", "attempt_ended"]
    );
    let sizes = tracker.book_sizes();
    assert_eq!((sizes.sessions, sizes.early_sessions), (0, 0));
}

/// PRD #1790 (A3): a producer flapping between two ids writes at most
/// [`MAX_SESSION_LINES`] lines while the attempt lives and no flush round
/// runs, and one more when it ends, so the attempt's last line is still its
/// latest id.
#[test]
fn a_flapping_session_is_bounded_and_its_latest_id_is_kept() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .resolve_spawn(None, Some("p"), None, None)
        .kept()
        .unwrap();
    let claude = AgentType::ClaudeCode;
    tracker.attempt_started(&work, "9", Some("p"), None);
    let flaps = 10 * MAX_SESSION_LINES;
    for n in 0..flaps {
        let id = if n % 2 == 0 { "s-a" } else { "s-b" };
        tracker.session_seen("9", Some("p"), id, &claude, false);
    }
    // The last flap reported `s-b`; then one new id arrives past the cap.
    tracker.session_seen("9", Some("p"), "s-last", &claude, false);
    let live = sessions_of(&lines(&tracker, state.path()), &work);
    assert_eq!(live.len(), MAX_SESSION_LINES, "bounded while it lives");
    tracker.attempt_ended("9", worker_end());
    let read = lines(&tracker, state.path());
    let all = sessions_of(&read, &work);
    assert_eq!(all.len(), MAX_SESSION_LINES + 1);
    assert_eq!(all.last().unwrap().1, "s-last", "the latest id is written");
    assert_eq!(
        crate::work_journal::latest_session_of(&read, &AttemptId::new("run1", 9).unwrap())
            .map(|(session, _)| session),
        Some("s-last")
    );
    // A held id equal to the last one written is not written again.
    let other = tracker
        .resolve_spawn(None, Some("q"), None, None)
        .kept()
        .unwrap();
    tracker.attempt_started(&other, "10", Some("q"), None);
    for n in 0..(MAX_SESSION_LINES + 1) {
        tracker.session_seen("10", Some("q"), &format!("s-{n}"), &claude, false);
    }
    tracker.session_seen(
        "10",
        Some("q"),
        &format!("s-{}", MAX_SESSION_LINES - 1),
        &claude,
        false,
    );
    tracker.attempt_ended("10", worker_end());
    let read = lines(&tracker, state.path());
    assert_eq!(sessions_of(&read, &other).len(), MAX_SESSION_LINES);
}

/// PRD #1790 (A3): `attempt_session` events take at most a share of the
/// journal's writer queue, so lifecycle events still find room while a
/// session id churns; an id the quota refused is held and written when the
/// attempt ends.
#[test]
fn session_churn_cannot_fill_the_writer_queue() {
    let state = crate::test_temp::tempdir().unwrap();
    let gate = crate::work_journal::WriterGate::closed();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "run1",
        &DeckId::generate(),
        JournalOptions {
            home: PathBuf::from("/home/someone"),
            queue_capacity: 16,
            hooks: crate::work_journal::TestHooks {
                gate: Some(Arc::clone(&gate)),
                ..Default::default()
            },
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let tracker = WorkTracker::default();
    tracker.install(journal);
    let claude = AgentType::ClaudeCode;
    let start = |agent: u64| {
        let pane = format!("p{agent}");
        let work = tracker
            .resolve_spawn(None, Some(&pane), None, None)
            .kept()
            .unwrap();
        tracker.attempt_started(&work, &agent.to_string(), Some(&pane), None);
        work
    };
    // The writer is held: its first event waits at the gate, the rest queue.
    let churning = start(0);
    for n in 0..10 {
        tracker.session_seen("0", Some("p0"), &format!("s-{n}"), &claude, false);
    }
    // Five more launches, ten lifecycle events: the queue still has room.
    let others: Vec<WorkId> = (1..=5).map(start).collect();
    gate.open();
    tracker.journal().unwrap().flush();
    for agent in 0..=5u64 {
        tracker.attempt_ended(&agent.to_string(), worker_end());
    }
    let read = lines(&tracker, state.path());
    for work in &others {
        let kinds = kinds_of(&read, work);
        assert_eq!(
            kinds,
            ["opened", "attempt_started", "attempt_ended"],
            "a lifecycle event was crowded out"
        );
    }
    let sessions = sessions_of(&read, &churning);
    assert!(
        sessions.len() <= 16 / crate::work_journal::SESSION_QUEUE_SHARE + 1,
        "{sessions:?}"
    );
    assert_eq!(
        sessions.last().unwrap().1,
        "s-9",
        "the latest id is written at the end"
    );
}

/// PRD #1790: Pi's synthetic `<pane>-session` id is not a session Pi could be
/// resumed into, so it is never written; an OpenCode id is written as
/// reported.
#[test]
fn a_synthetic_pi_session_is_not_written() {
    assert!(!is_resumable_session(
        &AgentType::Pi,
        Some("p"),
        "p-session"
    ));
    assert!(!is_resumable_session(&AgentType::Pi, Some("p"), "real-id"));
    assert!(!is_resumable_session(
        &AgentType::ClaudeCode,
        Some("p"),
        "p-session"
    ));
    assert!(!is_resumable_session(&AgentType::ClaudeCode, Some("p"), ""));
    assert!(is_resumable_session(
        &AgentType::OpenCode,
        Some("p"),
        "ses_abc"
    ));
    assert!(is_resumable_session(
        &AgentType::ClaudeCode,
        None,
        "p-session"
    ));

    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .resolve_spawn(None, Some("p"), None, None)
        .kept()
        .unwrap();
    tracker.attempt_started(&work, "3", Some("p"), None);
    tracker.session_seen("3", Some("p"), "p-session", &AgentType::Pi, false);
    tracker.session_seen("3", Some("p"), "ses_abc", &AgentType::OpenCode, false);
    let read = lines(&tracker, state.path());
    assert_eq!(
        sessions_of(&read, &work),
        [("run1/3".to_string(), "ses_abc".to_string())]
    );
}

/// PRD #1790: a spawn through the registry records how it was launched — the
/// command as given, the absolute folder, the display name and the
/// orchestration seat — under the journal's state directory, keyed by the
/// attempt's journal ids.
#[tokio::test]
async fn a_spawn_records_how_it_was_launched() {
    let state = crate::test_temp::tempdir().unwrap();
    let cwd = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let membership = TabMembership::Orchestration {
        name: "tdd".into(),
        role_index: 1,
        role_name: "coder".into(),
        is_start_role: false,
        orchestration_cwd: None,
        display_title: None,
        orchestration_id: Some("orch-7".into()),
    };
    let cwd_text = cwd.path().to_string_lossy().into_owned();
    let agent = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(&cwd_text),
            display_name: Some("Named pane"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), "lr-pane".to_string())],
            tab_membership: Some(membership),
            agent_type: Some(AgentType::ClaudeCode),
            ..SpawnOptions::default()
        })
        .expect("spawn");
    let work = registry
        .attest_facts(&agent)
        .and_then(|facts| facts.work)
        .expect("the work is pinned on the generation");
    let records = crate::launch_records::read_all(state.path()).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record.work, work);
    assert_eq!(
        record.attempt,
        AttemptId::new("run1", agent.parse().unwrap()).unwrap()
    );
    assert_eq!(record.command.as_deref(), Some("cat"));
    assert_eq!(record.cwd.as_deref(), Some(cwd_text.as_str()));
    assert_eq!(record.name.as_deref(), Some("Named pane"));
    assert_eq!(record.agent_type, Some(AgentType::ClaudeCode));
    assert_eq!(record.role.as_deref(), Some("coder"));
    assert_eq!(record.orchestration.as_deref(), Some("tdd"));
    assert_eq!(record.orchestration_id.as_deref(), Some("orch-7"));
    registry.shutdown_all();
}

/// PRD #1790 (R5): a journal read that skipped a line of a kind this build
/// does not know cannot show that a work is gone, since that line may be the
/// one naming it, so no launch record is pruned on it; with the line gone,
/// the old record of a forgotten work is.
#[test]
fn launch_records_are_kept_while_the_read_skipped_an_unknown_kind() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let journal = tracker.journal().unwrap();
    let work = tracker.mint().unwrap();
    let record = crate::launch_records::LaunchRecord {
        v: crate::launch_records::SCHEMA_VERSION,
        work: work.clone(),
        attempt: AttemptId::new("r9", 1).unwrap(),
        command: Some("cat".into()),
        shell: None,
        agent_type: None,
        cwd: Some("/abs".into()),
        cwd_moved_from: None,
        name: None,
        role: None,
        orchestration: None,
        orchestration_id: None,
        ts_ms: 0,
    };
    crate::launch_records::write(state.path(), &record).unwrap();
    let planted = state
        .path()
        .join(crate::work_journal::DIR_NAME)
        .join("run-r9.jsonl");
    std::fs::write(
        &planted,
        format!(
            "{{\"v\":1,\"seq\":1,\"ts_ms\":1,\"run\":\"r9\",\"kind\":\"some_future_kind\",\"work\":\"{work}\"}}\n"
        ),
    )
    .unwrap();
    let read = journal.read().unwrap();
    assert_eq!(read.stats.unknown_kind, 1, "precondition: {:?}", read.stats);
    assert!(launch_read_incomplete(&read.stats));
    prune_launch_records(journal);
    assert_eq!(
        crate::launch_records::read_all(state.path()).unwrap(),
        vec![record]
    );
    // Control: once no line the read cannot understand is left, it goes.
    std::fs::remove_file(&planted).unwrap();
    prune_launch_records(journal);
    assert!(
        crate::launch_records::read_all(state.path())
            .unwrap()
            .is_empty()
    );
}

/// PRD #1790 (R2): a spawn that gives no folder starts in the child's `HOME`,
/// here an env overlay's, and its launch record keeps that folder as an
/// absolute path, while the registry's own `cwd` stays what the caller gave
/// (nothing).
#[tokio::test]
async fn a_spawn_without_a_folder_records_the_home_it_started_in() {
    let state = crate::test_temp::tempdir().unwrap();
    let home = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let home_text = home.path().to_string_lossy().into_owned();
    let agent = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: None,
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.to_string(), "home-pane".to_string()),
                ("HOME".to_string(), home_text.clone()),
            ],
            ..SpawnOptions::default()
        })
        .expect("spawn");
    let records = crate::launch_records::read_all(state.path()).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].cwd.as_deref(), Some(home_text.as_str()));
    let listed = registry
        .agent_records()
        .into_iter()
        .find(|r| r.id == agent)
        .expect("the agent is listed");
    assert_eq!(
        listed.cwd, None,
        "the registry's cwd is what the caller gave"
    );
    registry.shutdown_all();
}

/// The latest `attempt_session` of `attempt` that a daemon started after
/// this one would read from `state`'s journal on disk.
fn latest_on_disk(state: &Path, attempt: &AttemptId) -> Option<String> {
    let read = read_journal(state).unwrap();
    crate::work_journal::latest_session_of(&read.lines, attempt).map(|(s, _)| s.to_string())
}

/// PRD #1790 (F1): an attempt whose session changed more often than
/// [`MAX_SESSION_LINES`] and is still live has its latest id on disk after
/// one [`WorkTracker::flush_held_sessions`] round, without ending: the
/// daemon dropped after that round leaves a journal whose latest session
/// for the attempt is the newest id. A round with nothing new held writes
/// nothing.
#[test]
fn a_live_attempts_latest_session_survives_a_restart() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let work = tracker
        .resolve_spawn(None, Some("p"), None, None)
        .kept()
        .unwrap();
    let claude = AgentType::ClaudeCode;
    tracker.attempt_started(&work, "9", Some("p"), None);
    let changes = MAX_SESSION_LINES + 2;
    for n in 0..changes {
        tracker.session_seen("9", Some("p"), &format!("s-{n}"), &claude, false);
        tracker.journal().unwrap().flush();
    }
    let attempt = AttemptId::new("run1", 9).unwrap();
    let newest = format!("s-{}", changes - 1);
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        Some(format!("s-{}", MAX_SESSION_LINES - 1)),
        "precondition: past the cap the newest id is held, not written at once"
    );
    assert_eq!(tracker.flush_held_sessions(), 1);
    assert_eq!(tracker.flush_held_sessions(), 0, "nothing new is held");
    // An id reported again after the round is not written again.
    tracker.session_seen("9", Some("p"), &newest, &claude, false);
    assert_eq!(tracker.flush_held_sessions(), 0);
    tracker.drain(Duration::from_secs(5));
    drop(tracker);
    assert_eq!(latest_on_disk(state.path(), &attempt), Some(newest));
    let read = read_journal(state.path()).unwrap();
    assert_eq!(sessions_of(&read.lines, &work).len(), MAX_SESSION_LINES + 1);
    assert!(
        !kinds_of(&read.lines, &work).contains(&"attempt_ended"),
        "the attempt was not ended"
    );
}

/// PRD #1790 (F1): a live attempt's first session report, refused by the
/// journal's session quota while the queue is full of other attempts'
/// session events, and followed by no further hook, is on disk once the
/// queue has drained and a [`WorkTracker::flush_held_sessions`] round ran.
#[test]
fn a_quota_refused_session_is_written_once_the_queue_drains() {
    let state = crate::test_temp::tempdir().unwrap();
    let gate = crate::work_journal::WriterGate::closed();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "run1",
        &DeckId::generate(),
        JournalOptions {
            home: PathBuf::from("/home/someone"),
            queue_capacity: 8,
            hooks: crate::work_journal::TestHooks {
                gate: Some(Arc::clone(&gate)),
                ..Default::default()
            },
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let tracker = WorkTracker::default();
    tracker.install(journal);
    let claude = AgentType::ClaudeCode;
    let start = |agent: &str| {
        let pane = format!("p{agent}");
        let work = tracker
            .resolve_spawn(None, Some(&pane), None, None)
            .kept()
            .unwrap();
        tracker.attempt_started(&work, agent, Some(&pane), None);
        work
    };
    let quiet = start("1");
    let _busy = start("2");
    // The writer waits at the gate; attempt 2's ids take the whole quota.
    let quota = 8 / crate::work_journal::SESSION_QUEUE_SHARE;
    for n in 0..quota {
        tracker.session_seen("2", Some("p2"), &format!("b-{n}"), &claude, false);
    }
    // Attempt 1's one and only report is refused by the quota and held.
    tracker.session_seen("1", Some("p1"), "s-only", &claude, false);
    gate.open();
    tracker.journal().unwrap().flush();
    let attempt = AttemptId::new("run1", 1).unwrap();
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        None,
        "precondition: the quota refused the report"
    );
    assert_eq!(tracker.flush_held_sessions(), 1);
    tracker.drain(Duration::from_secs(5));
    drop(tracker);
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        Some("s-only".to_string())
    );
    let read = read_journal(state.path()).unwrap();
    assert_eq!(
        sessions_of(&read.lines, &quiet),
        [("run1/1".to_string(), "s-only".to_string())]
    );
}

/// PRD #1790 (F1): the daemon's periodic round runs the flush every
/// [`SESSION_FLUSH_EVERY`], not before, and stops once its closure says the
/// registry is gone.
#[tokio::test(start_paused = true)]
async fn the_session_flush_runs_every_round_until_the_registry_is_gone() {
    let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&rounds);
    let task =
        spawn_session_flush(move || counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2);
    let load = || rounds.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(SESSION_FLUSH_EVERY / 2).await;
    assert_eq!(load(), 0, "no round before the first interval");
    tokio::time::sleep(SESSION_FLUSH_EVERY).await;
    assert_eq!(load(), 1);
    tokio::time::sleep(SESSION_FLUSH_EVERY * 4).await;
    assert_eq!(load(), 3, "it stopped once the closure answered false");
    task.await.unwrap();
}

/// Spawn `cat` on `pane` through `registry` on a blocking thread, paused
/// after its fork and before it publishes; answers the reserved generation's
/// id, the release, and the spawn's handle.
async fn reserved_spawn(
    registry: &Arc<AgentPtyRegistry>,
    pane: &str,
) -> (
    String,
    std::sync::mpsc::Sender<()>,
    tokio::task::JoinHandle<std::thread::Result<Result<String, crate::agent_pty::AgentPtyError>>>,
) {
    let (forked, publish) = registry.pause_next_publish_for_test();
    let spawning = tokio::task::spawn_blocking({
        let registry = Arc::clone(registry);
        let pane = pane.to_string();
        move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.spawn_agent(SpawnOptions {
                    command: Some("cat"),
                    env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane)],
                    ..SpawnOptions::default()
                })
            }))
        }
    });
    tokio::task::spawn_blocking(move || {
        forked
            .recv_timeout(Duration::from_secs(10))
            .expect("the child never forked")
    })
    .await
    .unwrap();
    let ids = registry.reserved_ids_for_test();
    assert_eq!(ids.len(), 1, "{ids:?}");
    (ids[0].clone(), publish, spawning)
}

/// PRD #1790 (F1): a session reported through the registry for a reserved
/// generation is held, and the registry's own reservation exits drop it in
/// the lock hold that ends the reservation: the post-fork refusal
/// (`abandon_locked`, here a shutdown latched while the spawn was paused)
/// and the reservation's `Drop` (a panic before publication). Each is read
/// from the tracker's book after the spawn returned; a published control
/// writes the held id instead.
#[tokio::test]
async fn the_registry_drops_held_sessions_with_every_unpublished_reservation() {
    let claude = AgentType::ClaudeCode;

    // The reservation's `Drop`: a panic between the fork and publication.
    let state = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    registry.inject_fault_for_test(
        crate::agent_pty::FAULT_SPAWN_BEFORE_PUBLISH,
        crate::agent_pty::TestFault::Panic("injected"),
    );
    let (id, publish, spawning) = reserved_spawn(&registry, "held-drop").await;
    registry.session_seen(&id, Some("held-drop"), "s-dropped", &claude);
    assert_eq!(registry.work().book_sizes().early_sessions, 1);
    publish.send(()).unwrap();
    assert!(spawning.await.unwrap().is_err(), "the fault did not fire");
    assert_eq!(registry.work().book_sizes().early_sessions, 0, "Drop");
    registry.shutdown_all();

    // `abandon_locked`: the shutdown latch refuses the publication.
    let state = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let (id, publish, spawning) = reserved_spawn(&registry, "held-abandon").await;
    registry.session_seen(&id, Some("held-abandon"), "s-abandoned", &claude);
    assert_eq!(registry.work().book_sizes().early_sessions, 1);
    registry.shutdown_all();
    publish.send(()).unwrap();
    assert!(
        spawning.await.unwrap().unwrap().is_err(),
        "the latched shutdown did not refuse it"
    );
    assert_eq!(
        registry.work().book_sizes().early_sessions,
        0,
        "abandon_locked"
    );
    assert!(read_journal(state.path()).unwrap().lines.iter().all(
        |l| !matches!(&l.event, Event::AttemptSession { session, .. } if session == "s-abandoned")
    ));

    // Control: the same spawn published writes the held id after its start.
    let state = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let (id, publish, spawning) = reserved_spawn(&registry, "held-kept").await;
    registry.session_seen(&id, Some("held-kept"), "s-kept", &claude);
    publish.send(()).unwrap();
    assert_eq!(spawning.await.unwrap().unwrap().unwrap(), id);
    assert_eq!(registry.work().book_sizes().early_sessions, 0);
    registry.work().drain(Duration::from_secs(5));
    let attempt = AttemptId::new("run1", id.parse().unwrap()).unwrap();
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        Some("s-kept".to_string())
    );
    registry.shutdown_all();
}

/// PRD #1790 (F1): the early-session book is bounded by the registry's
/// reservations, not by a count of its own: every reserved generation keeps
/// its first id (more of them than the 64 the book used to cap at), and the
/// call the registry makes when a reservation ends without an attempt (an
/// abandoned spawn, or one published serving no work) removes its entry.
#[test]
fn every_reserved_generation_keeps_its_first_session_until_its_reservation_ends() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let claude = AgentType::ClaudeCode;
    for n in 0..100 {
        tracker.session_seen(&format!("{}", 100 + n), Some("p"), "s-first", &claude, true);
    }
    assert_eq!(tracker.book_sizes().early_sessions, 100);
    let work = tracker
        .resolve_spawn(None, Some("p"), None, None)
        .kept()
        .unwrap();
    tracker.attempt_started(&work, "199", Some("p"), None);
    let read = lines(&tracker, state.path());
    assert_eq!(
        sessions_of(&read, &work),
        [("run1/199".to_string(), "s-first".to_string())],
        "the last of them still wrote its first id"
    );
    for n in 0..99 {
        tracker.spawn_abandoned(&format!("{}", 100 + n));
    }
    assert_eq!(tracker.book_sizes().early_sessions, 0);
}

/// PRD #1790 (F4): a prepared start (entered through its verified
/// directory's descriptor, on Linux as a `/proc/self/fd/N` alias) records
/// the path it was verified against. Read after the verified handle is
/// dropped, the record's folder is that path, which still names the
/// directory, and no descriptor alias.
#[cfg(unix)]
#[tokio::test]
async fn a_prepared_start_records_the_verified_path_not_its_descriptor() {
    let state = crate::test_temp::tempdir().unwrap();
    let root = crate::test_temp::tempdir().unwrap();
    let dir = root.path().join("prepared");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let verified = crate::project_resolve::VerifiedProjectDir::open(&dir).unwrap();
    registry
        .spawn_agent_in(
            SpawnOptions {
                command: Some("cat"),
                cwd: Some(&path),
                env: vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    "prepared-lr".to_string(),
                )],
                ..SpawnOptions::default()
            },
            &verified,
        )
        .expect("the prepared start is served");
    drop(verified);
    let records = crate::launch_records::read_all(state.path()).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let recorded = records[0].cwd.as_deref().expect("a folder is recorded");
    assert_eq!(recorded, path);
    assert!(!recorded.starts_with("/proc/"), "{recorded}");
    assert!(std::path::Path::new(recorded).is_dir());
    registry.shutdown_all();
}

/// PRD #1790 (G2): a prepared start whose verified directory was renamed
/// away, with a fresh directory put at its path, before the spawn: the
/// child runs in the verified directory (its marker lands there), and the
/// persisted launch record names that directory where it now is, not the
/// replacement at the requested path.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_prepared_start_whose_directory_moved_records_where_the_child_ran() {
    let state = crate::test_temp::tempdir().unwrap();
    let root = crate::test_temp::tempdir().unwrap();
    let dir = root.path().join("prepared");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.to_string_lossy().into_owned();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let verified = crate::project_resolve::VerifiedProjectDir::open(&dir).unwrap();
    let moved = root.path().join("prepared.moved");
    std::fs::rename(&dir, &moved).unwrap();
    std::fs::create_dir(&dir).unwrap();
    registry
        .spawn_agent_in(
            SpawnOptions {
                command: Some("echo x > marker; exec cat"),
                cwd: Some(&path),
                env: vec![
                    (DOT_AGENT_DECK_PANE_ID.to_string(), "moved-lr".to_string()),
                    ("SHELL".to_string(), "/bin/sh".to_string()),
                ],
                ..SpawnOptions::default()
            },
            &verified,
        )
        .expect("the prepared start is served");
    drop(verified);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !moved.join("marker").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the child never wrote its marker in the verified directory"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !dir.join("marker").exists(),
        "the child ran in the replacement"
    );
    let records = crate::launch_records::read_all(state.path()).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let recorded = records[0].cwd.as_deref().expect("a folder is recorded");
    assert_eq!(
        std::fs::canonicalize(recorded).unwrap(),
        std::fs::canonicalize(&moved).unwrap(),
        "the record names where the child ran, not the replacement at {path}"
    );
    assert_eq!(records[0].cwd_moved_from, None);
    registry.shutdown_all();
}

/// PRD #1790 (G3): an orderly shutdown writes a live attempt's held-back
/// session id itself. Past [`MAX_SESSION_LINES`] the newest id is held;
/// the agent's PTY reader is held before it ends the attempt (the exit
/// telemetry seam), and the daemon shuts down gracefully before any
/// periodic round. The journal on disk then names the newest id, with no
/// `attempt_ended` written by the held reader.
#[cfg(unix)]
#[tokio::test]
async fn a_graceful_shutdown_writes_the_held_session_before_its_drain() {
    let state = crate::test_temp::tempdir().unwrap();
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal(state.path(), "run1"));
    let pane = "held-at-shutdown";
    let agent = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
            ..SpawnOptions::default()
        })
        .expect("spawn");
    let claude = AgentType::ClaudeCode;
    let changes = MAX_SESSION_LINES + 1;
    for n in 0..changes {
        registry.session_seen(&agent, Some(pane), &format!("s-{n}"), &claude);
    }
    let attempt = AttemptId::new("run1", agent.parse().unwrap()).unwrap();
    registry.work().journal().unwrap().flush();
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        Some(format!("s-{}", MAX_SESSION_LINES - 1)),
        "precondition: past the cap the newest id is held, not written"
    );
    let (_reached, release) = registry.hold_exit_telemetry_for_test(&agent);
    let stopping = Arc::clone(&registry);
    tokio::task::spawn_blocking(move || stopping.shutdown_all_graceful(Duration::from_secs(2)))
        .await
        .unwrap();
    let read = read_journal(state.path()).unwrap();
    assert!(
        !read.lines.iter().any(|l| l.event.kind() == "attempt_ended"),
        "the held reader ended the attempt, so this did not test the shutdown"
    );
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        Some(format!("s-{}", changes - 1)),
        "the graceful shutdown did not write the held session id"
    );
    drop(release);
}

/// A journal whose writer waits at its gate, with a queue of 8: the tests
/// below fill it until it drops an event.
fn stalled_journal(state: &Path) -> (WorkJournal, Arc<crate::work_journal::WriterGate>) {
    let gate = crate::work_journal::WriterGate::closed();
    let journal = WorkJournal::try_open_with(
        state,
        "run1",
        &DeckId::generate(),
        JournalOptions {
            home: PathBuf::from("/home/someone"),
            queue_capacity: 8,
            hooks: crate::work_journal::TestHooks {
                gate: Some(Arc::clone(&gate)),
                ..Default::default()
            },
            ..JournalOptions::default()
        },
    )
    .unwrap();
    (journal, gate)
}

/// PRD #1790 (review round 4): the shutdown barrier ([`WorkTracker::drain`])
/// against a stalled writer and a full queue returns at its deadline instead
/// of waiting for queue room, and keeps the held session id it could not
/// queue: nothing of it is on disk once the writer is let go, and the next
/// [`WorkTracker::flush_held_sessions`] round still has it to write.
#[test]
fn a_drain_against_a_full_stalled_queue_returns_and_keeps_the_held_session() {
    let state = crate::test_temp::tempdir().unwrap();
    let (journal, gate) = stalled_journal(state.path());
    let tracker = WorkTracker::default();
    tracker.install(journal);
    let claude = AgentType::ClaudeCode;
    let start = |agent: &str| {
        let pane = format!("p{agent}");
        let work = tracker
            .resolve_spawn(None, Some(&pane), None, None)
            .kept()
            .unwrap();
        tracker.attempt_started(&work, agent, Some(&pane), None);
    };
    start("1");
    // The writer takes attempt 1's first event and parks at the gate with it
    // before the queue is filled. Otherwise a writer thread scheduled late
    // pops one event after the queue is full, and the drain rightly queues
    // the held id into the slot that frees.
    let parked = std::time::Instant::now();
    while gate.reached.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            parked.elapsed() < Duration::from_secs(10),
            "the writer never reached its gate"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    start("2");
    // Attempt 2's ids take the whole session quota, so attempt 1's one
    // report is refused and held.
    for n in 0..8 / crate::work_journal::SESSION_QUEUE_SHARE {
        tracker.session_seen("2", Some("p2"), &format!("b-{n}"), &claude, false);
    }
    tracker.session_seen("1", Some("p1"), "s-only", &claude, false);
    // Saturate the queue with lifecycle events. The quota's refusal above
    // already counts as a drop, so wait for one more.
    let journal = tracker.journal().unwrap();
    let refused = journal.dropped_events();
    assert!(
        refused > 0,
        "precondition: the quota refused attempt 1's id"
    );
    let mut n = 100;
    while journal.dropped_events() == refused {
        start(&n.to_string());
        n += 1;
        assert!(n < 200, "the queue never filled");
    }
    let bound = Duration::from_millis(200);
    let began = std::time::Instant::now();
    tracker.drain(bound);
    let took = began.elapsed();
    gate.open();
    assert!(
        took < bound + Duration::from_secs(2),
        "the drain waited {took:?} against a stalled writer (bound {bound:?})"
    );
    journal.flush();
    let attempt = AttemptId::new("run1", 1).unwrap();
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        None,
        "the drain could not have queued the held id, yet it is on disk"
    );
    assert_eq!(
        tracker.flush_held_sessions(),
        1,
        "the drain lost the held id it could not queue"
    );
    journal.flush();
    assert_eq!(
        latest_on_disk(state.path(), &attempt),
        Some("s-only".to_string())
    );
}

/// PRD #1790 (review round 4): a graceful shutdown of the registry whose
/// journal writer is stalled behind a full queue, with a held session id
/// still to hand over, returns within its agents' grace and the journal
/// drain bound ([`crate::work_journal::SHUTDOWN_DRAIN`]) rather than
/// hanging on the journal.
#[cfg(unix)]
#[tokio::test]
async fn a_graceful_shutdown_with_a_stalled_full_journal_returns_in_bound() {
    let state = crate::test_temp::tempdir().unwrap();
    let (journal, gate) = stalled_journal(state.path());
    let registry = Arc::new(AgentPtyRegistry::new());
    registry.work().install(journal);
    let spawn = |n: usize| {
        registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), format!("wj-stall-{n}"))],
                ..SpawnOptions::default()
            })
            .expect("spawn")
    };
    let agent = spawn(0);
    let claude = AgentType::ClaudeCode;
    for n in 0..=8 / crate::work_journal::SESSION_QUEUE_SHARE {
        registry.session_seen(&agent, Some("wj-stall-0"), &format!("s-{n}"), &claude);
    }
    // The quota's refusal above already counts as a drop, so saturate the
    // queue until one more is dropped.
    let journal = registry.work().journal().unwrap();
    let refused = journal.dropped_events();
    let mut n = 1;
    while journal.dropped_events() == refused {
        spawn(n);
        n += 1;
        assert!(n < 20, "the queue never filled");
    }
    let grace = Duration::from_secs(2);
    let began = std::time::Instant::now();
    let stopping = Arc::clone(&registry);
    tokio::task::spawn_blocking(move || stopping.shutdown_all_graceful(grace))
        .await
        .unwrap();
    let took = began.elapsed();
    gate.open();
    let bound = grace + crate::work_journal::SHUTDOWN_DRAIN + Duration::from_secs(3);
    assert!(
        took < bound,
        "the graceful shutdown took {took:?} against a stalled journal (bound {bound:?})"
    );
}

/// PRD #1790: the launch-record prune's liveness test is the journal's own
/// [`WorkJournal::run_is_live`], read from real run locks: this journal's
/// run and a run whose lock another open journal holds are live, so their
/// crash-left temporary files stay; a run whose lock file exists but is
/// free, and a run with no lock file, are not, so theirs go.
#[test]
fn pending_cleanup_reads_real_held_free_and_missing_run_locks() {
    let state = crate::test_temp::tempdir().unwrap();
    let tracker = tracker(state.path());
    let held = journal(state.path(), "run2");
    let journal_dir = state.path().join(crate::work_journal::DIR_NAME);
    std::fs::write(journal_dir.join("run-run3.lock"), b"").unwrap();
    assert!(!journal_dir.join("run-run4.lock").exists());
    let own = tracker.journal().unwrap();
    assert!(own.run_is_live("run1"), "this journal's own run");
    assert!(own.run_is_live("run2"), "a lock another journal holds");
    assert!(!own.run_is_live("run3"), "a free lock");
    assert!(!own.run_is_live("run4"), "no lock file");
    let work = tracker.mint().unwrap();
    crate::launch_records::write(
        state.path(),
        &crate::launch_records::LaunchRecord {
            v: crate::launch_records::SCHEMA_VERSION,
            work,
            attempt: AttemptId::new("run1", 1).unwrap(),
            command: None,
            shell: None,
            agent_type: None,
            cwd: None,
            cwd_moved_from: None,
            name: None,
            role: None,
            orchestration: None,
            orchestration_id: None,
            ts_ms: crate::launch_records::now_ms(),
        },
    )
    .unwrap();
    let records = state.path().join(crate::launch_records::DIR_NAME);
    let pending = |run: &str| records.join(format!(".pending-0123456789abcdef-{run}-4.json"));
    for run in ["run1", "run2", "run3", "run4"] {
        std::fs::write(pending(run), b"{").unwrap();
    }
    prune_launch_records(own);
    assert!(pending("run1").exists(), "this run's own temporary file");
    assert!(pending("run2").exists(), "a held run's temporary file");
    assert!(
        !pending("run3").exists(),
        "a free-lock run's temporary file"
    );
    assert!(!pending("run4").exists(), "a lockless run's temporary file");
    drop(held);
}
