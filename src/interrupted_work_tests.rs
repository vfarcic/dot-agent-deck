//! PRD #1790 stage 2: the interrupted work derivation's unit tests. Every
//! journal is built in memory; nothing touches a file, sleeps or reaches the
//! network.

use std::path::{Path, PathBuf};

use super::*;
use crate::launch_records::{LaunchRecord, SCHEMA_VERSION};
use crate::retained_store::{AdoptedState, KeptReport, OwnState};
use crate::work_journal::{Line, ReadStats};
use spec::spec;

const RESTORE_SESSION: &str = "01234567-89ab-4cde-8fab-0123456789ab";

fn restore_record(w: &WorkId, a: &AttemptId, agent: crate::event::AgentType) -> LaunchRecord {
    let mut r = record(w, a, "saved agent", Some("/home/someone/code/p"));
    r.agent_type = Some(agent);
    r
}

fn restore_session(
    j: &mut Journal,
    w: &WorkId,
    a: &AttemptId,
    agent: crate::event::AgentType,
    id: &str,
) {
    j.push(
        a.run(),
        Event::AttemptSession {
            work: w.clone(),
            attempt: a.clone(),
            session: id.into(),
            agent,
        },
    );
}

fn restore_verdict(agent: &ItemAgent) -> serde_json::Value {
    let verdict: &RestoreVerdict = &agent.restore;
    serde_json::to_value(verdict).unwrap()
}

/// Scenario: List an interrupted Claude or Codex pane, or a dispatched single agent, with a recorded conversation. The list names that agent and offers resume using its latest session, without checking whether its folder exists.
#[spec("journal/restore/001")]
#[test]
fn journal_restore_001_single_agents_offer_resume_from_the_latest_session() {
    use crate::event::AgentType;
    for (launch, kind) in [
        (Launch::Pane, ItemKind::Agent),
        (Launch::Dispatch, ItemKind::Unit),
        (Launch::IssueDispatch, ItemKind::IssueDispatch),
    ] {
        for agent_type in [AgentType::ClaudeCode, AgentType::Codex] {
            let mut j = Journal::default();
            let (w, a) = (work(1), attempt(OLD, 1));
            j.opened(
                &w,
                launch,
                None,
                Refs::default(),
                Some("/home/someone/code/p"),
            )
            .started(&w, &a);
            restore_session(&mut j, &w, &a, agent_type.clone(), "old-invalid-id");
            restore_session(&mut j, &w, &a, agent_type.clone(), RESTORE_SESSION);
            j.ended(&w, &a, EndHow::Interrupted);
            // Reader order is not session order: the higher seq wins.
            j.lines.swap(2, 3);
            let list = derive_in(&j, vec![restore_record(&w, &a, agent_type.clone())], &[]);
            assert_eq!(list.items.len(), 1);
            let item = &list.items[0];
            assert_eq!(item.kind, kind);
            assert_eq!(item.agents.len(), 1);
            assert_eq!(item.agents_omitted, 0);
            let agent: &ItemAgent = &item.agents[0];
            assert_eq!(agent.attempt.to_string(), a.to_string());
            assert_eq!(agent.name.as_deref(), Some("saved agent"));
            assert_eq!(agent.agent_type, Some(agent_type));
            assert_eq!(agent.role, None);
            assert_eq!(agent.folder.as_deref(), Some("/home/someone/code/p"));
            assert_eq!(
                restore_verdict(agent),
                serde_json::json!({"kind": "resume"})
            );
            let mut old_reply = serde_json::to_value(item).unwrap();
            old_reply.as_object_mut().unwrap().remove("agents");
            old_reply.as_object_mut().unwrap().remove("agents_omitted");
            let old_item: InterruptedItem = serde_json::from_value(old_reply).unwrap();
            assert!(
                old_item.agents.is_empty(),
                "older daemons omit the additive fields"
            );
            assert_eq!(old_item.agents_omitted, 0);
        }
    }
}

/// Scenario: List an interrupted team with twenty distinct agent attempts. The item carries at most sixteen agents and reports how many were omitted, so the restored-agent list remains bounded.
#[spec("journal/restore/001")]
#[test]
fn journal_restore_001_agent_lists_are_bounded_with_an_omitted_count() {
    use crate::event::AgentType;
    let mut j = Journal::default();
    let w = work(1);
    j.opened(&w, Launch::Orchestration, None, Refs::default(), None);
    let mut records = Vec::new();
    for n in 1..=20 {
        let a = attempt(OLD, n);
        j.started(&w, &a).ended(&w, &a, EndHow::Interrupted);
        records.push(restore_record(&w, &a, AgentType::ClaudeCode));
    }
    let list = derive_in(&j, records, &[]);
    let item = &list.items[0];
    assert_eq!(item.agents.len(), 16);
    assert_eq!(item.agents_omitted, 4);
    let attempts: std::collections::HashSet<_> = item
        .agents
        .iter()
        .map(|agent| agent.attempt.to_string())
        .collect();
    assert_eq!(attempts.len(), 16, "distinct attempts, not duplicate rows");
}

/// Scenario: List interrupted agents that cannot resume, including a default shell, and list a team's roles and multi-attempt units. Fresh starts are offered for the unsupported single agents; team roles and units outside the single-agent scope say orchestration-later.
#[spec("journal/restore/002")]
#[test]
fn journal_restore_002_gaps_start_over_and_teams_wait_for_orchestration_restore() {
    use crate::event::AgentType;
    for agent_type in [
        AgentType::Pi,
        AgentType::OpenCode,
        AgentType::Devin,
        AgentType::Antigravity,
        AgentType::None,
    ] {
        let mut j = Journal::default();
        let (w, a) = pane(&mut j, 1);
        restore_session(&mut j, &w, &a, agent_type.clone(), RESTORE_SESSION);
        j.ended(&w, &a, EndHow::Interrupted);
        let mut r = restore_record(&w, &a, agent_type.clone());
        r.command = crate::agent_registry::spec(&agent_type)
            .default_command
            .map(str::to_string);
        if agent_type == AgentType::None {
            r.agent_type = None;
        }
        let list = derive_in(&j, vec![r], &[]);
        let reason = if agent_type == AgentType::None {
            "default-shell"
        } else {
            "agent-cannot-resume"
        };
        assert_eq!(
            restore_verdict(&list.items[0].agents[0]),
            serde_json::json!({"kind": "start_over", "reason": reason})
        );
    }

    for launch in [
        Launch::Orchestration,
        Launch::Dispatch,
        Launch::IssueDispatch,
    ] {
        let mut j = Journal::default();
        let w = work(1);
        j.opened(&w, launch, None, Refs::default(), None);
        let mut records = Vec::new();
        for (n, role) in [(1, "orchestrator"), (2, "coder")] {
            let a = attempt(OLD, n);
            j.push(
                OLD,
                Event::AttemptStarted {
                    work: w.clone(),
                    attempt: a.clone(),
                    pane: role.into(),
                    agent: n,
                    role: Some(role.into()),
                },
            );
            restore_session(&mut j, &w, &a, AgentType::ClaudeCode, RESTORE_SESSION);
            j.ended(&w, &a, EndHow::Interrupted);
            let mut r = restore_record(&w, &a, AgentType::ClaudeCode);
            r.role = Some(role.into());
            records.push(r);
        }
        let list = derive_in(&j, records, &[]);
        assert_eq!(list.items[0].agents.len(), 2);
        for agent in &list.items[0].agents {
            assert!(matches!(
                agent.role.as_deref(),
                Some("orchestrator" | "coder")
            ));
            assert_eq!(
                restore_verdict(agent),
                serde_json::json!({"kind": "none", "reason": "orchestration-later"})
            );
        }
    }

    // Even one distinct attempt is outside Stage 3 once it has a delegation.
    let mut j = Journal::default();
    let (w, a) = (work(1), attempt(OLD, 1));
    j.opened(&w, Launch::Dispatch, None, Refs::default(), None)
        .started(&w, &a);
    let delegated = work(2);
    j.opened(
        &delegated,
        Launch::Delegation,
        Some(&w),
        Refs::default(),
        None,
    )
    .started(&delegated, &a);
    restore_session(&mut j, &w, &a, AgentType::ClaudeCode, RESTORE_SESSION);
    j.ended(&w, &a, EndHow::Interrupted);
    let list = derive_in(&j, vec![restore_record(&w, &a, AgentType::ClaudeCode)], &[]);
    assert_eq!(
        list.items[0].agents.len(),
        1,
        "a delegated generation is listed once"
    );
    assert_eq!(
        restore_verdict(&list.items[0].agents[0]),
        serde_json::json!({"kind": "none", "reason": "orchestration-later"})
    );
}

/// Scenario: List interrupted panes with ambiguous, absent or moved launch records, an invalid or absent conversation id, and incomplete reads. Each agent remains visible with a specific refusal or fresh-start reason instead of being offered resume.
#[spec("journal/restore/003")]
#[test]
fn journal_restore_003_bad_evidence_never_offers_resume() {
    use crate::event::AgentType;
    for (case, kind, reason) in [
        ("duplicate", "none", "ambiguous-record"),
        ("wrong-work", "none", "ambiguous-record"),
        ("shared-attempt", "none", "ambiguous-record"),
        ("moved", "none", "folder-moved"),
        ("missing", "none", "no-launch-record"),
        ("invalid", "start_over", "session-id-invalid"),
        ("no-session", "start_over", "no-session-id"),
        ("journal-incomplete", "none", "incomplete-read"),
        ("launch-incomplete", "none", "incomplete-read"),
        ("launch-skipped", "none", "incomplete-read"),
        ("journal-newer", "none", "incomplete-read"),
        ("journal-unknown-kind", "none", "incomplete-read"),
    ] {
        let mut j = Journal::default();
        let (w, a) = pane(&mut j, 1);
        if case != "no-session" {
            restore_session(
                &mut j,
                &w,
                &a,
                AgentType::ClaudeCode,
                if case == "invalid" {
                    "--continue"
                } else {
                    RESTORE_SESSION
                },
            );
        }
        j.ended(&w, &a, EndHow::Interrupted);
        let mut r = restore_record(&w, &a, AgentType::ClaudeCode);
        if case == "moved" {
            r.cwd = None;
            r.cwd_moved_from = Some("/home/someone/code/p".into());
        }
        if case == "wrong-work" {
            r.work = work(99);
        }
        if case == "shared-attempt" {
            let other = work(2);
            j.opened(&other, Launch::Pane, None, Refs::default(), None)
                .started(&other, &a)
                .ended(&other, &a, EndHow::Interrupted);
        }
        let mut records = launches(match case {
            "duplicate" => vec![r.clone(), r],
            "missing" => Vec::new(),
            _ => vec![r],
        });
        j.truncated = case == "journal-incomplete";
        records.truncated = case == "launch-incomplete";
        records.skipped = usize::from(case == "launch-skipped");
        let mut read = j.read();
        read.stats.newer_version = usize::from(case == "journal-newer");
        read.stats.unknown_kind = usize::from(case == "journal-unknown-kind");
        let list = derive_with_home(&read, &records, live_only, &[], &home());
        let item = list.items.iter().find(|item| item.id == w).unwrap();
        assert_eq!(
            item.agents.len(),
            1,
            "{case}: the refused agent stays visible"
        );
        assert_eq!(
            restore_verdict(&item.agents[0]),
            serde_json::json!({"kind": kind, "reason": reason}),
            "{case}"
        );
        if case.ends_with("incomplete") || case == "launch-skipped" || case.starts_with("journal-")
        {
            assert!(list.incomplete, "{case}");
        }
    }
}

/// PRD #1790 stage 3 (review R5): with many interrupted agents, each still
/// gets the latest session of its own attempt, now read from the index built
/// in one pass rather than from a scan of the whole journal per agent.
#[test]
fn many_interrupted_agents_each_resume_their_own_latest_session() {
    use crate::event::AgentType;
    const AGENTS: u64 = 600;
    let session =
        |n: u64, generation: u64| format!("{n:08x}-{generation:04x}-4cde-8fab-0123456789ab");
    let mut j = Journal::default();
    let mut records = Vec::new();
    for n in 1..=AGENTS {
        let (w, a) = pane(&mut j, n);
        restore_session(&mut j, &w, &a, AgentType::ClaudeCode, &session(n, 1));
        restore_session(&mut j, &w, &a, AgentType::ClaudeCode, &session(n, 2));
        j.ended(&w, &a, EndHow::Interrupted);
        records.push(restore_record(&w, &a, AgentType::ClaudeCode));
    }
    let read = j.read();
    let list = derive_with_home(&read, &launches(records), live_only, &[], &home());
    assert_eq!(list.items.len(), AGENTS as usize);
    for n in 1..=AGENTS {
        let item = list.items.iter().find(|item| item.id == work(n)).unwrap();
        assert_eq!(item.agents.len(), 1);
        assert_eq!(item.agents[0].restore, RestoreVerdict::Resume, "agent {n}");
        assert_eq!(
            crate::work_journal::latest_session_of(&read.lines, &item.agents[0].attempt)
                .map(|(id, _)| id.to_string()),
            Some(session(n, 2)),
        );
    }
    let indexed = crate::work_journal::latest_sessions(&read.lines);
    assert_eq!(indexed.len(), AGENTS as usize);
    for (attempt, report) in indexed {
        assert_eq!(
            crate::work_journal::latest_session_of(&read.lines, attempt).map(|(id, _)| id),
            Some(report.session)
        );
    }
}

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
        pid: None,
        pid_start: None,
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
            "agents",
            "branch",
            "id",
            "issue",
            "kind",
            "last_entry",
            "launcher",
            "name",
            "progress",
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

    let show = wire(&AttachRequest::ShowInterrupted {
        id: "w-1".into(),
        report: None,
        reports_from: None,
    });
    assert_eq!(
        show,
        serde_json::json!({"op": "show-interrupted", "id": "w-1"})
    );
    assert!(matches!(
        back(show),
        AttachRequest::ShowInterrupted { id, report: None, reports_from: None } if id == "w-1"
    ));
    let one = wire(&AttachRequest::ShowInterrupted {
        id: "w-1".into(),
        report: Some("r-17".into()),
        reports_from: None,
    });
    assert_eq!(
        one,
        serde_json::json!({"op": "show-interrupted", "id": "w-1", "report": "r-17"})
    );
    assert!(matches!(
        back(one),
        AttachRequest::ShowInterrupted { report: Some(r), .. } if r == "r-17"
    ));

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
        dismissed_omitted: 0,
        refused_omitted: 0,
        stopped: None,
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

/// Reviewer 1: a report the inbox accepted (`reported destination=inbox`)
/// and later set aside when its caller went away (this run's own file in
/// state recovered) is listed under its producer's item without a restart;
/// one still pinned is not.
#[test]
fn an_inbox_report_set_aside_for_a_gone_caller_is_listed_without_a_restart() {
    let mut j = Journal::default();
    let unit = work(1);
    let a = attempt(LIVE, 1);
    j.opened(&unit, Launch::Dispatch, None, Refs::default(), None)
        .started(&unit, &a)
        .reported(&unit, "set-aside", Destination::Inbox, None)
        .reported(&unit, "still-pinned", Destination::Inbox, None)
        .ended(&unit, &a, EndHow::Exited)
        .closed(&unit, CloseHow::UnitEnded);
    let own = |id: &str, state: OwnState| {
        KeptReport::from_name(LIVE, &crate::retained_store::own_name(id), Some(state)).unwrap()
    };
    let kept = vec![
        own("set-aside", OwnState::Recovered(1_800_000_000)),
        own("still-pinned", OwnState::Pinned),
    ];
    assert!(kept[0].left_for_gone_caller());
    assert!(!kept[1].left_for_gone_caller());
    let list = derive_in(&j, Vec::new(), &kept);
    assert_eq!(ids(&list), vec![unit]);
    let reports: Vec<&str> = list.items[0]
        .reports
        .iter()
        .map(|r| r.report_id.as_str())
        .collect();
    assert_eq!(reports, vec!["set-aside"]);
}

/// Reviewer 5: an item's progress rolls up every work under it: distinct
/// attempts (a delegation's attempt is its worker's, counted once), the
/// roles they ran as in the order first seen, and the delegations opened
/// and closed as done.
#[test]
fn an_items_progress_rolls_up_attempts_roles_and_delegations() {
    let mut j = Journal::default();
    let team = work(1);
    let lead = attempt(OLD, 1);
    let coder = attempt(OLD, 2);
    let tester = attempt(OLD, 3);
    let with_role = |j: &mut Journal, w: &WorkId, a: &AttemptId, role: &str| {
        j.push(
            OLD,
            Event::AttemptStarted {
                work: w.clone(),
                attempt: a.clone(),
                pane: "pane".into(),
                agent: 1,
                role: Some(role.into()),
            },
        );
    };
    j.opened(&team, Launch::Orchestration, None, Refs::default(), None);
    with_role(&mut j, &team, &lead, "orchestrator");
    with_role(&mut j, &team, &coder, "coder");
    let delegate = |j: &mut Journal, n: u64, worker: &AttemptId, role: &str, done: bool| {
        let task = work(n);
        j.opened(
            &task,
            Launch::Delegation,
            Some(&team),
            Refs {
                role: Some(role.into()),
                ..Refs::default()
            },
            None,
        );
        with_role(j, &task, worker, role);
        if done {
            j.closed(&task, CloseHow::TaskDone);
        }
    };
    delegate(&mut j, 2, &coder, "coder", true);
    delegate(&mut j, 3, &coder, "coder", true);
    delegate(&mut j, 4, &tester, "tester", false);
    j.ended(&team, &lead, EndHow::Interrupted);

    let list = derive_in(&j, Vec::new(), &[]);
    assert_eq!(ids(&list), vec![team]);
    let progress = &list.items[0].progress;
    assert_eq!(
        progress,
        &Progress {
            attempts: 3,
            roles: vec!["orchestrator".into(), "coder".into(), "tester".into()],
            delegations: 3,
            delegations_done: 2,
        }
    );
    assert_eq!(
        progress_line(progress),
        "3 attempts; roles orchestrator, coder, tester; 2 of 3 delegations done"
    );

    // A plain pane: one attempt, no roles, no delegations.
    let mut j = Journal::default();
    let (w, a) = pane(&mut j, 9);
    j.ended(&w, &a, EndHow::Interrupted);
    let list = derive_in(&j, Vec::new(), &[]);
    assert_eq!(list.items[0].progress.attempts, 1);
    assert_eq!(progress_line(&list.items[0].progress), "1 attempt");
    assert_eq!(
        progress_line(&Progress {
            attempts: 2,
            roles: Vec::new(),
            delegations: 1,
            delegations_done: 0,
        }),
        "2 attempts; 0 of 1 delegation done"
    );
}

/// An item whose strings are as long as a launch record allows and made of
/// characters JSON escapes six times over, with many reports.
fn heavy_item(n: u64, reports: usize) -> InterruptedItem {
    let ugly = |len: usize| "\u{1}".repeat(len);
    InterruptedItem {
        id: work(n),
        kind: ItemKind::Unit,
        name: Some(ugly(MAX_FIELD_BYTES * 3)),
        issue: Some(n),
        repo: Some(ugly(MAX_FIELD_BYTES)),
        branch: Some(ugly(MAX_FIELD_BYTES)),
        worktree: Some(ugly(MAX_FIELD_BYTES)),
        launcher: Some(ugly(MAX_FIELD_BYTES)),
        last_entry: LastEntry {
            kind: "reported".into(),
            ts_ms: n,
        },
        reports: (0..reports)
            .map(|r| ItemReport {
                report_id: format!("r-{n}-{r}"),
                task_id: Some(ugly(MAX_FIELD_BYTES)),
                unit_id: Some(ugly(MAX_FIELD_BYTES)),
                kept_until_ms: Some(1),
            })
            .collect(),
        reports_omitted: 0,
        cut: false,
        progress: Progress::default(),
        git: None,
        agents: Vec::new(),
        agents_omitted: 0,
    }
}

/// Auditor 6: a list whose items' metadata and reports would serialise far
/// past the attach frame is fitted to the reply budget: it stays within it
/// (escaping included), each item keeps at most `MAX_LISTED_REPORTS` report
/// entries and counts the rest, a string past `MAX_FIELD_BYTES` is cut and
/// marked, and the items left out are counted. A tiny budget still answers
/// the list's own fields with every item counted as omitted.
#[test]
fn a_list_is_fitted_to_the_reply_budget() {
    let unfitted = InterruptedList {
        items: (0..40).map(|n| heavy_item(n, 40)).collect(),
        incomplete: false,
        omitted: 0,
    };
    let whole: usize = unfitted.items.iter().map(json_len).sum();
    assert!(whole > crate::daemon_protocol::MAX_FRAME_LEN, "{whole}");

    let mut list = unfitted.clone();
    fit_list(&mut list, MAX_REPLY_BYTES, GIT_FACTS_RESERVE);
    let size = json_len(&list);
    assert!(size <= MAX_REPLY_BYTES, "{size}");
    assert!(size + list.items.len() * GIT_FACTS_RESERVE <= MAX_REPLY_BYTES);
    assert!(!list.items.is_empty());
    assert_eq!(list.items.len() + list.omitted, 40);
    assert!(list.omitted > 0);
    for item in &list.items {
        assert_eq!(item.reports.len(), MAX_LISTED_REPORTS);
        assert_eq!(item.reports_omitted, 40 - MAX_LISTED_REPORTS);
        assert!(item.cut);
        assert!(item.name.as_ref().unwrap().len() <= MAX_FIELD_BYTES);
    }

    let mut small = unfitted.clone();
    fit_list(&mut small, 2_048, 0);
    assert!(small.items.is_empty());
    assert_eq!(small.omitted, 40);
    assert!(json_len(&small) <= 2_048);

    // An ordinary list is left as it was.
    let mut j = Journal::default();
    let (w, a) = pane(&mut j, 1);
    j.ended(&w, &a, EndHow::Interrupted);
    let plain = derive_in(&j, Vec::new(), &[]);
    let mut fitted = plain.clone();
    fit_list(&mut fitted, MAX_REPLY_BYTES, 0);
    assert_eq!(fitted, plain);
}

/// Auditor 6: a show of an item with thousands of reports and large bodies
/// stays within the reply budget: its report entries are cut at
/// `MAX_SHOWN_ENTRY_BYTES` and counted, and bodies that would not fit are
/// counted rather than sent.
#[test]
fn a_show_is_fitted_to_the_reply_budget() {
    let item = heavy_item(1, 1_000);
    let bodies: Vec<String> = item
        .reports
        .iter()
        .take(MAX_SHOWN_REPORTS)
        .map(|r| r.report_id.clone())
        .collect();
    let big = |report_id: &str| ShownReport {
        report_id: report_id.to_string(),
        body: Some("\u{1}".repeat(MAX_SHOWN_REPORT_BYTES as usize * 2)),
        cut: true,
        error: None,
    };
    let shown = fit_show(item.clone(), &bodies, false, MAX_REPLY_BYTES, big);
    let size = json_len(&shown);
    assert!(size <= MAX_REPLY_BYTES, "{size}");
    assert!(shown.item.reports_omitted > 0);
    assert_eq!(shown.item.reports.len() + shown.item.reports_omitted, 1_000);
    assert!(shown.bodies_omitted > 0, "these bodies cannot all fit");
    assert_eq!(
        shown.reports.len() + shown.bodies_omitted,
        MAX_SHOWN_REPORTS
    );
    assert!(shown.item.cut);
}

/// Auditor N1: a query that parses as a full work id names that item or no
/// item. When the item it named is gone (dismissed or reclaimed by another
/// client), another item whose name or branch is that id string is not
/// picked in its place.
#[test]
fn a_full_work_id_matches_its_item_exactly_or_nothing() {
    let gone = work(1);
    let mut j = Journal::default();
    let (w2, a2) = pane(&mut j, 2);
    j.ended(&w2, &a2, EndHow::Interrupted);
    let base = derive_in(&j, Vec::new(), &[]);
    assert_eq!(ids(&base), vec![w2.clone()]);
    let id_of = |r: Resolved<'_>| match r {
        Resolved::One(item) => Some(item.id.clone()),
        _ => None,
    };

    let mut named = base.clone();
    named.items[0].name = Some(gone.to_string());
    assert_eq!(resolve(&named, gone.as_str()), Resolved::NotListed);
    assert_eq!(
        resolve(&named, &format!("  {gone} ")),
        Resolved::NotListed,
        "surrounding blanks do not turn a full id into a name"
    );

    let mut branched = base.clone();
    branched.items[0].branch = Some(gone.to_string());
    assert_eq!(resolve(&branched, gone.as_str()), Resolved::NotListed);

    // The item is still found by its own id, and a name that is not a work
    // id still resolves.
    assert_eq!(id_of(resolve(&named, w2.as_str())), Some(w2.clone()));
    let mut plain = base.clone();
    plain.items[0].name = Some("fix-login".into());
    assert_eq!(id_of(resolve(&plain, "fix-login")), Some(w2));
}

/// Reviewer 5: an item's roles are in the order the journal first records
/// them, across all of its works: a delegation's role recorded before a
/// later root attempt's role comes first.
#[test]
fn an_items_roles_are_in_the_order_the_journal_first_records_them() {
    let mut j = Journal::default();
    let team = work(1);
    let lead = attempt(OLD, 1);
    let coder = attempt(OLD, 2);
    let task = work(2);
    let with_role = |j: &mut Journal, w: &WorkId, a: &AttemptId, role: &str| {
        j.push(
            OLD,
            Event::AttemptStarted {
                work: w.clone(),
                attempt: a.clone(),
                pane: "pane".into(),
                agent: 1,
                role: Some(role.into()),
            },
        );
    };
    j.opened(&team, Launch::Orchestration, None, Refs::default(), None);
    j.opened(
        &task,
        Launch::Delegation,
        Some(&team),
        Refs {
            role: Some("coder".into()),
            ..Refs::default()
        },
        None,
    );
    with_role(&mut j, &task, &coder, "coder");
    with_role(&mut j, &team, &lead, "tester");
    j.ended(&team, &lead, EndHow::Interrupted);

    let list = derive_in(&j, Vec::new(), &[]);
    assert_eq!(ids(&list), vec![team]);
    assert_eq!(
        list.items[0].progress.roles,
        vec!["coder".to_string(), "tester".to_string()]
    );
}

/// Auditor N2 / reviewer 2: a dismissed item's name is a display string
/// like any other a reply carries, so it is cut to `MAX_FIELD_BYTES`.
#[test]
fn a_dismissed_items_name_is_cut_to_the_field_bound() {
    let mut outcome = DismissOutcome::default();
    outcome.push_dismissed(DismissedItem {
        id: work(1).to_string(),
        name: Some("n".repeat(80 * 1024)),
    });
    let name = outcome.dismissed[0].name.as_deref().unwrap();
    assert!(name.len() <= MAX_FIELD_BYTES, "{}", name.len());
}

/// Auditor N2 / reviewer 2: a dismissal's answer is fitted to a byte budget,
/// refusals first: what it leaves out is counted, so the counts still add up
/// to everything the request did, and a tiny budget still answers its own
/// fields with every entry counted.
#[test]
fn a_dismissal_answer_is_fitted_to_a_byte_budget_counting_what_it_leaves_out() {
    let mut outcome = DismissOutcome::default();
    for n in 0..100 {
        outcome.push_dismissed(DismissedItem {
            id: work(n).to_string(),
            name: Some("\u{1}".repeat(MAX_FIELD_BYTES)),
        });
    }
    for n in 0..10 {
        outcome.push_refused(format!("no-such-{n}"), RefusalReason::NotListed);
    }
    let whole = json_len(&outcome);
    let budget = whole / 4;
    let mut fitted = outcome.clone();
    fitted.fit(budget);
    assert!(json_len(&fitted) <= budget, "{}", json_len(&fitted));
    assert_eq!(fitted.refused.len(), 10, "refusals are kept first");
    assert!(!fitted.dismissed.is_empty() && fitted.dismissed_omitted > 0);
    assert_eq!(fitted.dismissed.len() + fitted.dismissed_omitted, 100);
    assert_eq!(
        fitted.dismissed[..],
        outcome.dismissed[..fitted.dismissed.len()]
    );

    let mut tiny = outcome.clone();
    tiny.fit(64);
    assert!(tiny.dismissed.is_empty() && tiny.refused.is_empty());
    assert_eq!((tiny.dismissed_omitted, tiny.refused_omitted), (100, 10));

    let mut roomy = outcome.clone();
    roomy.fit(MAX_REPLY_BYTES);
    assert_eq!(roomy, outcome, "an answer that fits is left as it was");
}

/// Reviewer 1: a show's page is additive on the wire. The request carries
/// `reports_from` only when a page is asked for, and the answer carries
/// `reports_from` and `next_reports_from` only when they say something; an
/// answer without them decodes as the first page with no next one.
#[test]
fn a_report_page_is_additive_and_optional_on_the_wire() {
    use crate::daemon_protocol::AttachRequest;
    let paged = serde_json::to_value(AttachRequest::ShowInterrupted {
        id: "w-1".into(),
        report: None,
        reports_from: Some(16),
    })
    .unwrap();
    assert_eq!(
        paged,
        serde_json::json!({"op": "show-interrupted", "id": "w-1", "reports_from": 16})
    );
    assert!(matches!(
        serde_json::from_value::<AttachRequest>(paged).unwrap(),
        AttachRequest::ShowInterrupted {
            reports_from: Some(16),
            ..
        }
    ));
    let plain: AttachRequest =
        serde_json::from_str(r#"{"op": "show-interrupted", "id": "w-1"}"#).unwrap();
    assert!(matches!(
        plain,
        AttachRequest::ShowInterrupted {
            reports_from: None,
            ..
        }
    ));

    let item = heavy_item(1, 0);
    let first = fit_show(
        item.clone(),
        &[],
        false,
        MAX_REPLY_BYTES,
        |_| unreachable!(),
    );
    let wire = serde_json::to_value(&first).unwrap();
    assert!(wire.get("reports_from").is_none(), "{wire}");
    assert!(wire.get("next_reports_from").is_none(), "{wire}");
    let decoded: InterruptedShow = serde_json::from_value(wire).unwrap();
    assert_eq!((decoded.reports_from, decoded.next_reports_from), (0, None));

    let mut many = item;
    many.reports = (0..20)
        .map(|n| ItemReport {
            report_id: format!("r-{n}"),
            task_id: None,
            unit_id: None,
            kept_until_ms: None,
        })
        .collect();
    let page = fit_show_page(
        many,
        Some(16),
        &[],
        false,
        MAX_REPLY_BYTES,
        |_| unreachable!(),
    );
    let wire = serde_json::to_value(&page).unwrap();
    assert_eq!(wire["reports_from"], 16);
    assert!(
        wire.get("next_reports_from").is_none(),
        "the last page: {wire}"
    );
    assert_eq!(page.item.reports.len(), 4);
    assert_eq!(page.item.reports_omitted, 16);
}

/// Auditor wire check: the two lock verdicts are additive. Each serialises
/// as its own kebab-case value, and a reader that predates them (its enum
/// without them, ending in the same `#[serde(other)] Unknown`) decodes the
/// value as `Unknown` instead of failing the whole answer.
#[test]
fn the_lock_verdicts_decode_as_unknown_in_a_reader_that_predates_them() {
    #[derive(Debug, PartialEq, serde::Deserialize)]
    #[serde(rename_all = "kebab-case")]
    enum OlderVerdict {
        Removed,
        WouldRemove,
        KeptDirty,
        KeptHoldsWorktrees,
        KeptCouldNotCheck,
        RemoveFailed,
        TimedOut,
        KeptChanged,
        Refused,
        #[serde(other)]
        Unknown,
    }
    for (verdict, wire) in [
        (ReclaimVerdict::KeptBusy, "kept-busy"),
        (ReclaimVerdict::KeptCouldNotLock, "kept-could-not-lock"),
    ] {
        let json = serde_json::to_value(verdict).unwrap();
        assert_eq!(json, serde_json::json!(wire));
        let older: OlderVerdict = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(older, OlderVerdict::Unknown);
        let back: ReclaimVerdict = serde_json::from_value(json).unwrap();
        assert_eq!(back, verdict);
    }
}

/// Final review, reviewer 1 / auditor 2: the confirmed ids of a dismissal are
/// split into selectors the daemon admits — at most 256 ids, and at most
/// 32 KiB of them together — in order, each id in exactly one batch.
#[test]
fn dismiss_batches_fit_the_daemons_count_and_byte_limits() {
    use crate::close_agents::{MAX_SELECTOR_BYTES, MAX_SELECTOR_ENTRIES, MAX_SELECTOR_ENTRY_BYTES};
    assert!(dismiss_batches(&[]).is_empty());
    let short: Vec<String> = (0..257).map(|n| format!("w-{n}")).collect();
    let batches = dismiss_batches(&short);
    assert_eq!(
        batches.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![256, 1]
    );
    assert_eq!(batches.concat(), short);
    // Ids at the longest one may be: the byte limit, not the count, splits.
    let long: Vec<String> = (0..100)
        .map(|n| format!("{n:0>width$}", width = MAX_SELECTOR_ENTRY_BYTES))
        .collect();
    let batches = dismiss_batches(&long);
    assert!(
        batches.len() > 1,
        "{:?}",
        batches.iter().map(Vec::len).collect::<Vec<_>>()
    );
    for batch in &batches {
        assert!(batch.len() <= MAX_SELECTOR_ENTRIES);
        assert!(batch.iter().map(String::len).sum::<usize>() <= MAX_SELECTOR_BYTES);
    }
    assert_eq!(batches.concat(), long);
}

/// PRD #1790 stage 3: a resume that did not take (`restore_failed`) leaves
/// the agent listed with Start over offered; a restored or started-over item
/// (`closed how=restored|started_over`) is no longer listed.
#[test]
fn a_failed_resume_offers_start_over_and_a_restore_or_start_over_leaves_the_list() {
    use crate::event::AgentType;
    use crate::work_journal::{RestoreFailure, RestoredHow};
    let build = || {
        let mut j = Journal::default();
        let (w, a) = (work(1), attempt(OLD, 1));
        j.opened(
            &w,
            Launch::Pane,
            None,
            Refs::default(),
            Some("/home/someone/code/p"),
        )
        .started(&w, &a);
        restore_session(&mut j, &w, &a, AgentType::ClaudeCode, RESTORE_SESSION);
        j.ended(&w, &a, EndHow::Interrupted);
        (j, w, a)
    };
    let (mut j, w, a) = build();
    j.push(
        LIVE,
        Event::RestoreFailed {
            work: w.clone(),
            attempt: a.clone(),
            failure: RestoreFailure::SessionMismatch,
            shim: None,
        },
    );
    let list = derive_in(&j, vec![restore_record(&w, &a, AgentType::ClaudeCode)], &[]);
    assert_eq!(list.items.len(), 1);
    assert_eq!(
        restore_verdict(&list.items[0].agents[0]),
        serde_json::json!({"kind": "start_over", "reason": "not-resumed"})
    );
    for (how, restored) in [
        (CloseHow::Restored, RestoredHow::Resumed),
        (CloseHow::StartedOver, RestoredHow::StartedOver),
    ] {
        let (mut j, w, a) = build();
        j.closed(&w, how).push(
            LIVE,
            Event::AttemptRestored {
                work: w.clone(),
                from: a.clone(),
                to: attempt(LIVE, 1),
                how: restored,
            },
        );
        let list = derive_in(&j, vec![restore_record(&w, &a, AgentType::ClaudeCode)], &[]);
        assert!(list.items.is_empty(), "{how:?}: {list:?}");
    }
}
