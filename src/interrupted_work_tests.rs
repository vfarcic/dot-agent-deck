//! PRD #1790 stage 2: the interrupted work derivation's unit tests. Every
//! journal is built in memory; nothing touches a file, sleeps or reaches the
//! network.

use std::path::{Path, PathBuf};

use super::*;
use crate::launch_records::{LaunchRecord, SCHEMA_VERSION};
use crate::retained_store::{AdoptedState, KeptReport, OwnState};
use crate::work_journal::{Line, ReadStats};

const DECK: &str = "abcdefgh";
const OLD: &str = "runold";
const LIVE: &str = "runlive";

fn home() -> PathBuf {
    PathBuf::from("/home/someone")
}

fn work(n: u64) -> WorkId {
    WorkId::parse(&format!("w-{DECK}-{n:016x}")).unwrap()
}

fn attempt(run: &str, agent: u64) -> AttemptId {
    AttemptId::new(run, agent).unwrap()
}

fn workspace(path: &str) -> Workspace {
    Workspace::from_path_with_home(Path::new(path), &home()).unwrap()
}

/// A journal built line by line, one second apart.
#[derive(Default)]
struct Journal {
    lines: Vec<Line>,
    truncated: bool,
}

impl Journal {
    fn push(&mut self, run: &str, event: Event) -> &mut Self {
        let seq = self.lines.len() as u64 + 1;
        self.lines.push(Line {
            v: 1,
            seq,
            ts_ms: 1_000 * seq,
            run: run.to_string(),
            event,
        });
        self
    }

    fn opened(
        &mut self,
        w: &WorkId,
        launch: Launch,
        parent: Option<&WorkId>,
        refs: Refs,
        ws: Option<&str>,
    ) -> &mut Self {
        self.push(
            OLD,
            Event::Opened {
                work: w.clone(),
                launch,
                parent: parent.cloned(),
                refs: Box::new(refs),
                workspace: ws.map(workspace),
            },
        )
    }

    fn started(&mut self, w: &WorkId, a: &AttemptId) -> &mut Self {
        let run = a.run().to_string();
        self.push(
            &run,
            Event::AttemptStarted {
                work: w.clone(),
                attempt: a.clone(),
                pane: "pane".into(),
                agent: 1,
                role: None,
            },
        )
    }

    fn ended(&mut self, w: &WorkId, a: &AttemptId, how: EndHow) -> &mut Self {
        self.push(
            LIVE,
            Event::AttemptEnded {
                work: w.clone(),
                attempt: a.clone(),
                how,
                code: None,
                cause: None,
            },
        )
    }

    fn closed(&mut self, w: &WorkId, how: CloseHow) -> &mut Self {
        self.push(
            LIVE,
            Event::Closed {
                work: w.clone(),
                how,
            },
        )
    }

    fn reported(
        &mut self,
        w: &WorkId,
        report_id: &str,
        destination: Destination,
        to: Option<&WorkId>,
    ) -> &mut Self {
        let run = OLD.to_string();
        self.push(
            &run,
            Event::Reported {
                work: w.clone(),
                report_id: report_id.into(),
                task_id: Some(format!("task-{report_id}")),
                unit_id: None,
                destination,
                to: to.cloned(),
            },
        )
    }

    fn read(&self) -> JournalRead {
        JournalRead {
            lines: self.lines.clone(),
            stats: ReadStats {
                truncated: self.truncated,
                ..ReadStats::default()
            },
        }
    }
}

fn record(w: &WorkId, a: &AttemptId, name: &str, cwd: Option<&str>) -> LaunchRecord {
    LaunchRecord {
        v: SCHEMA_VERSION,
        work: w.clone(),
        attempt: a.clone(),
        command: Some("claude".into()),
        shell: None,
        agent_type: None,
        cwd: cwd.map(str::to_string),
        cwd_moved_from: None,
        name: Some(name.into()),
        role: None,
        orchestration: None,
        orchestration_id: None,
        ts_ms: 5,
    }
}

fn launches(records: Vec<LaunchRecord>) -> LaunchRead {
    LaunchRead {
        records,
        skipped: 0,
        truncated: false,
    }
}

fn live_only(run: &str) -> bool {
    run == LIVE
}

fn derive_in(
    journal: &Journal,
    records: Vec<LaunchRecord>,
    kept: &[KeptReport],
) -> InterruptedList {
    derive_with_home(
        &journal.read(),
        &launches(records),
        live_only,
        kept,
        &home(),
    )
}

fn ids(list: &InterruptedList) -> Vec<WorkId> {
    list.items.iter().map(|item| item.id.clone()).collect()
}

fn pane_refs(pane: &str) -> Refs {
    Refs {
        pane: Some(pane.into()),
        ..Refs::default()
    }
}

/// A plain pane work on the dead run with one attempt.
fn pane(j: &mut Journal, n: u64) -> (WorkId, AttemptId) {
    let (w, a) = (work(n), attempt(OLD, n));
    j.opened(
        &w,
        Launch::Pane,
        None,
        pane_refs(&format!("pane-{n}")),
        Some("/home/someone/code/p"),
    );
    j.started(&w, &a);
    (w, a)
}

#[test]
fn an_interrupted_work_is_listed_and_a_finished_or_closed_one_is_not() {
    let mut j = Journal::default();
    let (interrupted, a1) = pane(&mut j, 1);
    j.ended(&interrupted, &a1, EndHow::Interrupted);
    let (exited, a2) = pane(&mut j, 2);
    j.ended(&exited, &a2, EndHow::Exited);
    let (closed, a3) = pane(&mut j, 3);
    j.ended(&closed, &a3, EndHow::Interrupted)
        .closed(&closed, CloseHow::PaneClosed);
    let (dismissed, a4) = pane(&mut j, 4);
    j.ended(&dismissed, &a4, EndHow::Interrupted)
        .closed(&dismissed, CloseHow::Dismissed);
    let (reclaimed, a5) = pane(&mut j, 5);
    j.ended(&reclaimed, &a5, EndHow::Interrupted)
        .closed(&reclaimed, CloseHow::Reclaimed);

    let list = derive_in(&j, Vec::new(), &[]);
    assert_eq!(ids(&list), vec![interrupted.clone()]);
    let item = &list.items[0];
    assert_eq!(item.kind, ItemKind::Agent);
    assert_eq!(item.name.as_deref(), Some("pane-1"));
    assert_eq!(item.worktree.as_deref(), Some("/home/someone/code/p"));
    assert_eq!(item.launcher, None);
    assert!(!list.incomplete);
}

#[test]
fn a_live_runs_open_attempt_is_not_listed() {
    let mut j = Journal::default();
    // An attempt of a live run with no end: running, not interrupted.
    let (running, live) = (work(1), attempt(LIVE, 1));
    j.opened(&running, Launch::Pane, None, pane_refs("running"), None)
        .started(&running, &live);
    // A work interrupted before the restart and started again under the live
    // run: it is running again, so it is not listed either.
    let (resumed, before) = pane(&mut j, 2);
    let after = attempt(LIVE, 2);
    j.ended(&resumed, &before, EndHow::Interrupted)
        .started(&resumed, &after);

    let list = derive_in(&j, Vec::new(), &[]);
    assert!(list.items.is_empty(), "{list:?}");
}

#[test]
fn a_dead_runs_attempt_with_no_end_yet_counts_as_interrupted() {
    let mut j = Journal::default();
    // Startup recovery has not written the end yet.
    let (pending, _) = pane(&mut j, 1);

    let list = derive_in(&j, Vec::new(), &[]);
    assert_eq!(ids(&list), vec![pending]);
    assert_eq!(
        list.items[0].last_entry,
        LastEntry {
            kind: "attempt_started".into(),
            ts_ms: 2_000
        }
    );
}

#[test]
fn a_dispatch_with_an_orchestration_child_rolls_up_to_one_unit_item() {
    let mut j = Journal::default();
    // The dispatcher's pane: the launcher, listed as its own agent item.
    let (dispatcher, dispatcher_attempt) = pane(&mut j, 1);
    j.ended(&dispatcher, &dispatcher_attempt, EndHow::Interrupted);
    let unit = work(2);
    j.opened(
        &unit,
        Launch::Dispatch,
        Some(&dispatcher),
        Refs {
            branch: Some("agent/dispatch-fix".into()),
            unit_id: Some("u-1".into()),
            ..Refs::default()
        },
        Some("/home/someone/code/repo-fix"),
    );
    let team = work(3);
    j.opened(
        &team,
        Launch::Orchestration,
        Some(&unit),
        Refs {
            orchestration: Some("team".into()),
            orchestration_id: Some("o-1".into()),
            ..Refs::default()
        },
        Some("/home/someone/code/repo-fix"),
    );
    let (orchestrator, coder) = (attempt(OLD, 3), attempt(OLD, 4));
    j.started(&team, &orchestrator).started(&team, &coder);
    // A delegation to the coder, still open, and an earlier one that finished.
    let (task, done) = (work(4), work(5));
    let task_refs = Refs {
        task_id: Some("m-1".into()),
        ..Refs::default()
    };
    j.opened(
        &done,
        Launch::Delegation,
        Some(&team),
        task_refs.clone(),
        None,
    )
    .started(&done, &coder)
    .closed(&done, CloseHow::TaskDone);
    j.opened(&task, Launch::Delegation, Some(&team), task_refs, None)
        .started(&task, &coder);
    j.ended(&team, &orchestrator, EndHow::Interrupted)
        .ended(&team, &coder, EndHow::Interrupted);

    let records = vec![
        record(&dispatcher, &dispatcher_attempt, "dispatcher", None),
        // Joined although a delegation also holds the coder's attempt.
        record(&team, &coder, "coder", Some("/srv/real/repo-fix")),
        record(
            &team,
            &orchestrator,
            "orchestrator",
            Some("/home/someone/code/repo-fix"),
        ),
    ];
    let list = derive_in(&j, records, &[]);
    assert_eq!(ids(&list), vec![unit.clone(), dispatcher.clone()]);
    let item = &list.items[0];
    assert_eq!(item.kind, ItemKind::Unit);
    assert_eq!(item.branch.as_deref(), Some("agent/dispatch-fix"));
    assert_eq!(item.name.as_deref(), Some("team"));
    assert_eq!(item.launcher.as_deref(), Some("dispatcher"));
    // The cwd that redacts to the workspace is preferred; the other is not
    // the same place.
    assert_eq!(
        item.worktree.as_deref(),
        Some("/home/someone/code/repo-fix")
    );
    assert_eq!(list.items[1].kind, ItemKind::Agent);

    assert_eq!(
        works_under(&j.read(), &unit),
        vec![unit.clone(), team.clone(), task.clone()]
    );
}

#[test]
fn a_scheduled_issue_dispatch_names_its_issue_and_schedule() {
    let mut j = Journal::default();
    let fire = work(1);
    j.opened(
        &fire,
        Launch::ScheduleFire,
        None,
        Refs {
            schedule: Some("nightly".into()),
            ..Refs::default()
        },
        None,
    )
    .closed(&fire, CloseHow::FireDone);
    let issue = work(2);
    let a = attempt(OLD, 2);
    j.opened(
        &issue,
        Launch::IssueDispatch,
        Some(&fire),
        Refs {
            issue: Some(7),
            repo: Some("owner/name".into()),
            branch: Some("agent/issue-7".into()),
            ..Refs::default()
        },
        Some("/home/someone/code/issue-7"),
    )
    .started(&issue, &a)
    .ended(&issue, &a, EndHow::Interrupted);

    let list = derive_in(&j, Vec::new(), &[]);
    assert_eq!(ids(&list), vec![issue]);
    let item = &list.items[0];
    assert_eq!(item.kind, ItemKind::IssueDispatch);
    assert_eq!(
        (item.issue, item.repo.as_deref()),
        (Some(7), Some("owner/name"))
    );
    assert_eq!(item.launcher.as_deref(), Some("nightly"));
    assert_eq!(item.worktree.as_deref(), Some("/home/someone/code/issue-7"));
}

#[test]
fn an_ambiguous_or_mismatched_launch_record_is_ignored() {
    let mut j = Journal::default();
    let (twice, a1) = pane(&mut j, 1);
    let (mismatched, a2) = pane(&mut j, 2);
    let (joined, a3) = pane(&mut j, 3);
    for (w, a) in [(&twice, &a1), (&mismatched, &a2), (&joined, &a3)] {
        j.ended(w, a, EndHow::Interrupted);
    }
    let records = vec![
        // Two records for one attempt.
        record(&twice, &a1, "first", None),
        record(&twice, &a1, "second", None),
        // A record naming a work the journal does not hold its attempt under.
        record(&joined, &a2, "wrong-work", None),
        record(&joined, &a3, "right", None),
    ];
    let list = derive_in(&j, records, &[]);
    let name = |w: &WorkId| {
        list.items
            .iter()
            .find(|item| &item.id == w)
            .and_then(|item| item.name.clone())
    };
    assert_eq!(name(&twice).as_deref(), Some("pane-1"));
    assert_eq!(name(&mismatched).as_deref(), Some("pane-2"));
    assert_eq!(name(&joined).as_deref(), Some("right"));
}

#[test]
fn a_truncated_journal_or_launch_read_marks_the_list_incomplete() {
    let mut j = Journal::default();
    pane(&mut j, 1);
    assert!(!derive_in(&j, Vec::new(), &[]).incomplete);
    j.truncated = true;
    assert!(derive_in(&j, Vec::new(), &[]).incomplete);
    j.truncated = false;
    let cut = LaunchRead {
        truncated: true,
        ..LaunchRead::default()
    };
    assert!(derive_with_home(&j.read(), &cut, live_only, &[], &home()).incomplete);
}

#[test]
fn reports_left_for_a_gone_caller_are_joined_and_acknowledged_ones_are_not() {
    let mut j = Journal::default();
    // A unit that finished before the restart: it is not interrupted, but its
    // reports were never read.
    let unit = work(1);
    let a = attempt(OLD, 1);
    j.opened(
        &unit,
        Launch::Dispatch,
        None,
        Refs {
            branch: Some("agent/dispatch-done".into()),
            ..Refs::default()
        },
        None,
    )
    .started(&unit, &a);
    for id in ["acked", "pinned", "recovered", "unlisted", "missing"] {
        j.reported(&unit, id, Destination::Inbox, None);
    }
    j.ended(&unit, &a, EndHow::Exited)
        .closed(&unit, CloseHow::UnitEnded);
    // A delegation in the live run whose orchestrator was gone: its report
    // went to recovery and is held in this run's own file.
    let team = work(2);
    let task = work(3);
    let coder = attempt(LIVE, 3);
    j.opened(&team, Launch::Orchestration, None, Refs::default(), None)
        .opened(
            &task,
            Launch::Delegation,
            Some(&team),
            Refs::default(),
            None,
        )
        .started(&task, &coder)
        .reported(&task, "set-aside", Destination::Recovery, Some(&team))
        .reported(&task, "delivered", Destination::Inbox, Some(&team))
        .closed(&task, CloseHow::TaskDone);
    // A dismissed unit's unread report is not listed again.
    let gone = work(4);
    j.opened(&gone, Launch::Dispatch, None, Refs::default(), None)
        .reported(&gone, "dismissed-report", Destination::Inbox, None)
        .closed(&gone, CloseHow::UnitEnded)
        .closed(&gone, CloseHow::Dismissed);

    let adopted = |id: &str, state: AdoptedState| {
        let name = crate::retained_store::adopted_name(id, 1_900_000_000, state);
        KeptReport::from_name(LIVE, &name, None).unwrap()
    };
    let own = |id: &str, state: OwnState| {
        KeptReport::from_name(LIVE, &crate::retained_store::own_name(id), Some(state)).unwrap()
    };
    let kept = vec![
        adopted("acked", AdoptedState::Acknowledged),
        adopted("pinned", AdoptedState::Pinned),
        adopted("recovered", AdoptedState::Recovered),
        adopted("unlisted", AdoptedState::Unlisted),
        adopted("dismissed-report", AdoptedState::Pinned),
        own("set-aside", OwnState::Recovered(1_800_000_000)),
        own("delivered", OwnState::Pinned),
    ];
    let list = derive_in(&j, Vec::new(), &kept);
    assert_eq!(ids(&list), vec![team.clone(), unit.clone()]);
    let reports = |w: &WorkId| -> Vec<String> {
        list.items
            .iter()
            .find(|item| &item.id == w)
            .unwrap()
            .reports
            .iter()
            .map(|r| r.report_id.clone())
            .collect()
    };
    assert_eq!(reports(&unit), vec!["pinned", "recovered", "unlisted"]);
    assert_eq!(reports(&team), vec!["set-aside"]);
    let unit_item = list.items.iter().find(|item| item.id == unit).unwrap();
    assert_eq!(unit_item.kind, ItemKind::Unit);
    assert_eq!(unit_item.reports[0].kept_until_ms, Some(1_900_000_000_000));
    assert_eq!(unit_item.reports[0].task_id.as_deref(), Some("task-pinned"));
    let team_item = list.items.iter().find(|item| item.id == team).unwrap();
    assert_eq!(
        team_item.reports[0].kept_until_ms,
        Some((1_800_000_000 + 7 * 24 * 60 * 60) * 1000)
    );
}

#[test]
fn the_list_serialises_with_the_agreed_field_names() {
    let mut j = Journal::default();
    let (w, a) = pane(&mut j, 1);
    j.ended(&w, &a, EndHow::Interrupted);
    let mut list = derive_in(&j, Vec::new(), &[]);
    list.items[0].reports.push(ItemReport {
        report_id: "r1".into(),
        task_id: None,
        unit_id: Some("u-1".into()),
        kept_until_ms: Some(9),
    });
    let json = serde_json::to_value(&list).unwrap();
    assert_eq!(json["incomplete"], false);
    let item = &json["items"][0];
    let mut keys: Vec<&str> = item
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "branch",
            "id",
            "issue",
            "kind",
            "last_entry",
            "launcher",
            "name",
            "repo",
            "reports",
            "worktree"
        ]
    );
    assert_eq!(item["kind"], "agent");
    assert_eq!(item["id"], w.as_str());
    assert_eq!(item["last_entry"]["kind"], "attempt_ended");
    assert_eq!(
        item["reports"][0],
        serde_json::json!({"report_id": "r1", "unit_id": "u-1", "kept_until_ms": 9})
    );
    let back: InterruptedList = serde_json::from_value(json).unwrap();
    assert_eq!(back, list);
}

#[test]
fn a_workspace_resolves_to_a_matching_cwd_or_expands_under_home() {
    let ws = workspace("/home/someone/code/x");
    assert_eq!(ws.as_str(), "~/code/x");
    assert_eq!(
        workspace_path(&ws, ["/elsewhere/x", "/home/someone/code/x"], &home()).as_deref(),
        Some("/home/someone/code/x")
    );
    assert_eq!(
        workspace_path(&ws, [], &PathBuf::from("/home/other")).as_deref(),
        Some("/home/other/code/x")
    );
    assert_eq!(
        workspace("/home/someone").to_path_with_home(&home()),
        Some(home())
    );
    assert_eq!(
        workspace("/srv/x").to_path_with_home(&home()),
        Some(PathBuf::from("/srv/x"))
    );
    // A `~` with no home to expand it against names nothing.
    assert_eq!(ws.to_path_with_home(Path::new("/")), None);
}

#[test]
fn the_interrupted_work_requests_round_trip_with_their_ops_and_omit_their_defaults() {
    use crate::daemon_protocol::{AttachRequest, CallerClaim};
    let wire = |req: &AttachRequest| serde_json::to_value(req).unwrap();
    let back = |value: serde_json::Value| serde_json::from_value::<AttachRequest>(value).unwrap();

    let plain = wire(&AttachRequest::ListInterruptedWork { git: false });
    assert_eq!(plain, serde_json::json!({"op": "list-interrupted-work"}));
    assert!(matches!(
        back(plain),
        AttachRequest::ListInterruptedWork { git: false }
    ));
    let with_git = wire(&AttachRequest::ListInterruptedWork { git: true });
    // Parsed from text: linkage-check reads a bare `"git"` literal in `src/`
    // as a Git program name.
    let expected: serde_json::Value =
        serde_json::from_str(r#"{"op": "list-interrupted-work", "git": true}"#).unwrap();
    assert_eq!(with_git, expected);
    assert!(matches!(
        back(with_git),
        AttachRequest::ListInterruptedWork { git: true }
    ));

    let show = wire(&AttachRequest::ShowInterrupted { id: "w-1".into() });
    assert_eq!(
        show,
        serde_json::json!({"op": "show-interrupted", "id": "w-1"})
    );
    assert!(matches!(back(show), AttachRequest::ShowInterrupted { id } if id == "w-1"));

    let person = wire(&AttachRequest::ReclaimInterrupted {
        id: "w-1".into(),
        dry_run: false,
        caller: None,
    });
    assert_eq!(
        person,
        serde_json::json!({"op": "reclaim-interrupted", "id": "w-1"})
    );
    let agent = wire(&AttachRequest::ReclaimInterrupted {
        id: "w-1".into(),
        dry_run: true,
        caller: Some(CallerClaim {
            pane_id: "p".into(),
            token: "t".into(),
        }),
    });
    assert_eq!(agent["dry_run"], true);
    assert_eq!(agent["caller"]["pane_id"], "p");
    assert!(matches!(
        back(agent),
        AttachRequest::ReclaimInterrupted { dry_run: true, caller: Some(c), .. } if c.pane_id == "p"
    ));

    let all = wire(&AttachRequest::DismissInterrupted {
        ids: Vec::new(),
        all: true,
        caller: None,
    });
    assert_eq!(
        all,
        serde_json::json!({"op": "dismiss-interrupted", "all": true})
    );
    let some = back(serde_json::json!({"op": "dismiss-interrupted", "ids": ["a", "b"]}));
    assert!(matches!(
        some,
        AttachRequest::DismissInterrupted { ids, all: false, caller: None } if ids == ["a", "b"]
    ));
}

#[test]
fn the_interrupted_work_answers_are_additive_and_decode_unknown_values() {
    use crate::daemon_protocol::AttachResponse;
    let plain = serde_json::to_value(AttachResponse::ok()).unwrap();
    for key in [
        "interrupted_work",
        "interrupted_item",
        "interrupted_reclaim",
        "interrupted_dismiss",
    ] {
        assert!(plain.get(key).is_none(), "{key} is omitted when absent");
    }

    let outcome = ReclaimOutcome {
        id: Some("w-1".into()),
        dry_run: false,
        verdict: ReclaimVerdict::KeptHoldsWorktrees,
        message: reclaim_sentence(ReclaimVerdict::KeptHoldsWorktrees).into(),
        refusal: None,
        worktree: Some("/w".into()),
        branch: Some("b".into()),
        unpushed_commits: Some(3),
        nested_worktrees: vec!["/w/pr".into()],
        recorded: false,
    };
    let dismissed = DismissOutcome {
        dismissed: vec![DismissedItem {
            id: "w-2".into(),
            name: Some("unit".into()),
        }],
        refused: vec![DismissRefused {
            id: "x".into(),
            refusal: Refusal::new(RefusalReason::NotListed),
        }],
        refusal: None,
        incomplete: true,
    };
    let resp = AttachResponse {
        interrupted_reclaim: Some(outcome.clone()),
        interrupted_dismiss: Some(dismissed.clone()),
        ..AttachResponse::ok()
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(
        json["interrupted_reclaim"]["verdict"],
        "kept-holds-worktrees"
    );
    assert_eq!(
        json["interrupted_dismiss"]["refused"][0]["refusal"]["reason"],
        "not-listed"
    );
    let back: AttachResponse = serde_json::from_value(json).unwrap();
    assert_eq!(back.interrupted_reclaim, Some(outcome));
    assert_eq!(back.interrupted_dismiss, Some(dismissed));

    // A newer daemon's value decodes rather than failing the whole answer.
    let verdict: ReclaimVerdict = serde_json::from_value(serde_json::json!("kept-later")).unwrap();
    assert_eq!(verdict, ReclaimVerdict::Unknown);
    let reason: RefusalReason = serde_json::from_value(serde_json::json!("new-reason")).unwrap();
    assert_eq!(reason, RefusalReason::Unknown);
    let pr: PrFact = serde_json::from_value(serde_json::json!("draft")).unwrap();
    assert_eq!(pr, PrFact::Unknown);
    let facts: GitFacts = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(facts, GitFacts::default());
}

#[test]
fn an_id_resolves_exactly_then_by_unique_name_branch_or_suffix() {
    let mut j = Journal::default();
    let (w1, a1) = pane(&mut j, 1);
    j.ended(&w1, &a1, EndHow::Interrupted);
    let (w2, a2) = pane(&mut j, 2);
    j.ended(&w2, &a2, EndHow::Interrupted);
    let mut list = derive_in(&j, Vec::new(), &[]);
    for item in &mut list.items {
        if item.id == w1 {
            item.name = Some("alpha".into());
            item.branch = Some("agent/dispatch-alpha".into());
        } else {
            item.name = Some("alpha".into());
        }
    }
    let id_of = |r: Resolved<'_>| match r {
        Resolved::One(item) => Some(item.id.clone()),
        _ => None,
    };
    assert_eq!(id_of(resolve(&list, w2.as_str())), Some(w2.clone()));
    assert_eq!(
        id_of(resolve(&list, "agent/dispatch-alpha")),
        Some(w1.clone())
    );
    let suffix = &w2.as_str()[w2.as_str().len() - 6..];
    assert_eq!(id_of(resolve(&list, suffix)), Some(w2.clone()));
    assert_eq!(resolve(&list, "alpha"), Resolved::Ambiguous);
    assert_eq!(resolve(&list, "nothing"), Resolved::NotListed);
    assert_eq!(resolve(&list, "  "), Resolved::NotListed);
}

#[test]
fn the_shared_words_read_as_decided() {
    assert_eq!(INTERRUPTED_WORK_TITLE, "Interrupted work");
    assert_eq!(RECLAIM_ACTION, "Reclaim");
    assert_eq!(RECLAIM_EXPLANATION, "remove its worktree, keep its branch");
    assert_eq!(DISMISS_ACTION, "Dismiss");
    assert_eq!(DISMISS_ALL_ACTION, "Dismiss all");
    assert_eq!(
        interrupted_footer(3),
        "3 interrupted items from before the restart"
    );
    assert_eq!(
        interrupted_footer(1),
        "1 interrupted item from before the restart"
    );
    assert_eq!(unpushed_sentence(0), None);
    assert!(unpushed_sentence(2).unwrap().starts_with("2 commits"));
    assert_eq!(
        Refusal::new(RefusalReason::AgentCaller).message,
        REFUSED_AGENT_CALLER
    );
}

#[test]
fn still_interrupted_is_false_once_dismissed_or_resumed() {
    let mut j = Journal::default();
    let (w, a) = pane(&mut j, 1);
    j.ended(&w, &a, EndHow::Interrupted);
    assert!(still_interrupted(&j.read(), &w, live_only));
    let mut resumed = Journal::default();
    let (w2, a2) = pane(&mut resumed, 1);
    resumed.ended(&w2, &a2, EndHow::Interrupted);
    resumed.started(&w2, &attempt(LIVE, 9));
    assert!(!still_interrupted(&resumed.read(), &w2, live_only));
    j.closed(&w, CloseHow::Dismissed);
    assert!(!still_interrupted(&j.read(), &w, live_only));
}
