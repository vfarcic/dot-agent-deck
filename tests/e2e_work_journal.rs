#![cfg(all(feature = "e2e", unix))]

//! L2 coverage for the daemon's work journal (PRD #1889): every launch path
//! records a work with its parent, refs and workspace in
//! `<state_dir>/work-journal/run-<daemon instance id>.jsonl`, each agent
//! generation is an attempt of that work, every spawned agent is told its
//! work id in `DOT_AGENT_DECK_WORK_ID`, and a daemon that starts after one that
//! died marks the attempts that one left open as interrupted.
//!
//! Headless `daemon serve` with token-free stand-ins: the journal file and the
//! stand-in's own environment are the observable end-state, read from outside
//! the daemon.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/work_journal.rs"]
mod journal;
#[path = "support/inbox_route.rs"]
mod route;

use inbox::{Endpoint, Pane};
use serde_json::{Value, json};
use spec::spec;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The work id the deck gave `pane`'s process, as the routing stand-in wrote
/// it beside its pid (empty when the deck gave none).
fn work_env(pane: &Pane) -> String {
    let path = pane.capture.with_extension("work");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the stand-in's work-id file {}: {error}", path.display()))
}

fn str_of<'a>(line: &'a Value, key: &str) -> &'a str {
    line[key]
        .as_str()
        .unwrap_or_else(|| panic!("journal line has no string {key:?}: {line}"))
}

/// Every v1 line carries the envelope the contract names.
fn assert_envelope(line: &Value) {
    assert_eq!(line["v"], 1, "schema version: {line}");
    assert!(line["seq"].as_u64().is_some_and(|n| n >= 1), "seq: {line}");
    assert!(line["ts_ms"].as_u64().is_some(), "ts_ms: {line}");
    assert!(!str_of(line, "run").is_empty(), "run: {line}");
    assert!(str_of(line, "work").starts_with("w-"), "work id: {line}");
}

/// The routing stand-in's agent dispatched from `caller` as a `--single` unit
/// named `unit`, once the daemon has spawned it.
fn dispatch_single(endpoint: &impl Endpoint, caller: &Pane, repo: &Path, unit: &str) -> Pane {
    dispatch_as(endpoint, caller, repo, unit, "--single")
}

/// The first agent the daemon spawned for a dispatch of `unit` from `caller`
/// with the shape flag `shape` (`--single` or `--orchestration=<name>`).
fn dispatch_as(
    endpoint: &impl Endpoint,
    caller: &Pane,
    repo: &Path,
    unit: &str,
    shape: &str,
) -> Pane {
    inbox::status(endpoint, caller, "session_start", "codex");
    inbox::status(endpoint, caller, "thinking", "codex");
    route::cli(
        endpoint,
        caller,
        repo,
        &["dispatch", unit, "--task", "JOURNAL-DISPATCH-TASK", shape],
    );
    let suffix = format!("-dispatch-{unit}");
    let found = std::cell::RefCell::new(None);
    let budget = common::load_scaled(Duration::from_secs(20));
    assert!(
        common::wait_until(budget, || {
            *found.borrow_mut() = common::agent_records_on(endpoint.attach_socket())
                .into_iter()
                .find(|r| {
                    r.id != caller.agent && r.cwd.as_deref().is_some_and(|c| c.ends_with(&suffix))
                })
                .map(|r| r.id);
            found.borrow().is_some()
        }),
        "the dispatch never spawned its unit within {budget:?}: {:?}",
        common::agent_records_on(endpoint.attach_socket())
    );
    route::observe(endpoint, &found.into_inner().unwrap())
}

fn cwd_of(endpoint: &impl Endpoint, agent: &str) -> PathBuf {
    common::agent_records_on(endpoint.attach_socket())
        .into_iter()
        .find(|r| r.id == agent)
        .and_then(|r| r.cwd)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("agent {agent} has no recorded cwd"))
}

/// Scenario: Start a routing stand-in pane through a headless daemon in its own directory. The journal must record an `opened` work with `launch=pane`, that pane and its workspace, and an `attempt_started` naming the pane and its agent, and the stand-in's own `DOT_AGENT_DECK_WORK_ID` must equal that work's id. Closing the pane through the daemon must then write that attempt's `attempt_ended` and `closed how=pane_closed` on the pane's work.
#[spec("journal/launch/001")]
#[test]
fn journal_launch_001_a_pane_opens_a_work_its_agent_is_told() {
    let daemon = inbox::daemon_unbound(&[]);
    let state = journal::default_state_dir(&daemon);
    let ws = daemon.home.join("journal-pane-ws");
    std::fs::create_dir_all(&ws).unwrap();
    let pane = route::start(&daemon, "journal-pane", json!({"cwd": ws}));

    let opened = journal::wait_for(
        &state,
        "an `opened` launch=pane for pane journal-pane",
        |l| {
            journal::find(l, "opened", |o| {
                o["launch"] == "pane" && o["refs"]["pane"] == "journal-pane"
            })
            .cloned()
        },
    );
    assert_envelope(&opened);
    let work = str_of(&opened, "work").to_string();
    assert!(
        journal::workspace_is(&opened, &daemon.home, &ws),
        "workspace must be the pane's cwd {:?}: {opened}",
        journal::workspace_spellings(&daemon.home, &ws)
    );
    assert!(
        opened["parent"].is_null(),
        "a manual pane has no parent: {opened}"
    );

    let attempt = journal::wait_for(&state, "the pane's `attempt_started`", |l| {
        journal::attempt_of(l, &pane.agent).cloned()
    });
    assert_envelope(&attempt);
    assert_eq!(attempt["work"], work.as_str(), "attempt of the pane's work");
    assert_eq!(attempt["pane"], "journal-pane");
    assert_eq!(
        attempt["attempt"],
        format!("{}/{}", str_of(&opened, "run"), pane.agent),
        "attempt id is <run>/<agent id>"
    );

    assert_eq!(
        work_env(&pane),
        work,
        "the agent's DOT_AGENT_DECK_WORK_ID must be its pane's work id"
    );

    // Closing the pane through the daemon ends its attempt and closes its work.
    let attempt_id = str_of(&attempt, "attempt").to_string();
    stop(&daemon, &pane.agent);
    let ended = journal::wait_for(&state, "the closed pane's `attempt_ended`", |l| {
        journal::find(l, "attempt_ended", |e| e["attempt"] == attempt_id.as_str()).cloned()
    });
    assert_envelope(&ended);
    assert_eq!(ended["work"], work.as_str(), "{ended}");
    journal::wait_for(&state, "the pane work's `closed how=pane_closed`", |l| {
        journal::find(l, "closed", |c| {
            c["work"] == work.as_str() && c["how"] == "pane_closed"
        })
        .cloned()
    });
}

/// Close agent `agent` through the daemon (the attach `StopAgent` verb), as
/// the TUI's and the desktop's close does.
fn stop(endpoint: &impl Endpoint, agent: &str) {
    let reply = inbox::attach(endpoint, json!({"op":"stop-agent","id":agent}));
    assert_eq!(reply["ok"], true, "stop agent {agent}: {reply}");
}

/// Scenario: A routing stand-in pane in a one-commit repository runs the real `dot-agent-deck dispatch --single`; the journal must open a `launch=dispatch` work parented on the dispatcher pane's work, with the unit's branch, unit id and worktree, whose attempt and `DOT_AGENT_DECK_WORK_ID` are the unit agent's. The unit's `work-done --done` must write `reported` on that work with the unit id, a report id and `to` = the dispatcher's work. A second unit closed before it reports must get `closed how=unit_ended`.
#[spec("journal/launch/002")]
#[test]
fn journal_launch_002_a_dispatch_is_a_child_of_its_dispatchers_work() {
    const UNIT: &str = "journal-dispatch";
    const SILENT: &str = "journal-silent";
    let daemon = inbox::daemon_unbound(&[]);
    let state = journal::default_state_dir(&daemon);
    let repo = daemon.home.join("journal-repo");
    journal::init_repo(&repo);
    let caller = route::start(&daemon, "journal-caller", json!({"cwd": repo}));
    let unit = dispatch_single(&daemon, &caller, &repo, UNIT);
    let worktree = cwd_of(&daemon, &unit.agent);

    let caller_work = journal::wait_for(&state, "the dispatcher pane's `attempt_started`", |l| {
        journal::attempt_of(l, &caller.agent).map(|a| str_of(a, "work").to_string())
    });
    let opened = dispatch_opened(&state, UNIT);
    assert_envelope(&opened);
    let work = str_of(&opened, "work").to_string();
    assert_eq!(
        opened["parent"],
        caller_work.as_str(),
        "the dispatch's parent is the dispatcher pane's work: {opened}"
    );
    let unit_id = opened["refs"]["unit_id"]
        .as_str()
        .filter(|id| id.starts_with("u-"))
        .unwrap_or_else(|| panic!("refs.unit_id: {opened}"))
        .to_string();
    assert!(
        journal::workspace_is(&opened, &daemon.home, &worktree),
        "workspace must be the unit's worktree {:?}: {opened}",
        journal::workspace_spellings(&daemon.home, &worktree)
    );

    let attempt = journal::wait_for(&state, "the unit agent's `attempt_started`", |l| {
        journal::attempt_of(l, &unit.agent).cloned()
    });
    assert_eq!(
        attempt["work"],
        work.as_str(),
        "a single unit's generation is an attempt of the dispatch work: {attempt}"
    );
    assert_eq!(
        work_env(&unit),
        work,
        "the unit agent's DOT_AGENT_DECK_WORK_ID must be the dispatch work id"
    );

    // The unit's terminal report is `reported` on the dispatch work, addressed
    // to the dispatcher's work.
    route::cli(
        &daemon,
        &unit,
        &worktree,
        &["work-done", "--done", "--task", route::REPORT],
    );
    let reported = journal::wait_for(&state, "the unit's terminal `reported`", |l| {
        journal::find(l, "reported", |r| r["work"] == work.as_str()).cloned()
    });
    assert_envelope(&reported);
    assert_eq!(reported["unit_id"], unit_id.as_str(), "{reported}");
    assert_eq!(reported["to"], caller_work.as_str(), "{reported}");
    assert_eq!(reported["destination"], "inbox", "{reported}");
    assert!(
        reported["report_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{reported}"
    );

    // A sibling unit closed before it reports anything closes `unit_ended`.
    let silent = dispatch_single(&daemon, &caller, &repo, SILENT);
    let silent_open = dispatch_opened(&state, SILENT);
    let silent_work = str_of(&silent_open, "work").to_string();
    assert_eq!(silent_open["parent"], caller_work.as_str(), "{silent_open}");
    journal::wait_for(&state, "the silent unit's `attempt_started`", |l| {
        journal::attempt_of(l, &silent.agent).cloned()
    });
    stop(&daemon, &silent.agent);
    journal::wait_for(&state, "the silent unit's `closed how=unit_ended`", |l| {
        journal::find(l, "closed", |c| {
            c["work"] == silent_work.as_str() && c["how"] == "unit_ended"
        })
        .cloned()
    });
    let lines = journal::lines(&state);
    assert!(
        journal::find(&lines, "reported", |r| r["work"] == silent_work.as_str()).is_none(),
        "a unit that never reported has no `reported`:\n{}",
        journal::dump(&state)
    );
    let text = journal::dump(&state);
    assert!(
        !text.contains("ROUTE-REPORT-b3d8") && !text.contains("JOURNAL-DISPATCH-TASK"),
        "the journal must hold no task or report text:\n{text}"
    );
}

/// The `opened launch=dispatch` of the unit named `unit` (its branch is
/// `agent/dispatch-<unit>`).
fn dispatch_opened(state: &Path, unit: &str) -> Value {
    let branch = format!("agent/dispatch-{unit}");
    journal::wait_for(
        state,
        &format!("an `opened` launch=dispatch on {branch}"),
        |l| {
            journal::find(l, "opened", |o| {
                o["launch"] == "dispatch" && o["refs"]["branch"] == branch.as_str()
            })
            .cloned()
        },
    )
}

/// Scenario: Start a two-role routing orchestration in the home directory whose worker clears on delegate, mark the worker idle, delegate a task and report it through the respawned worker with `work-done --task-id`. The journal must open the orchestration work (workspace `~`) with its id and name, a `launch=delegation` child with the task id and role, a `reported` and a `task_done` close on the delegation, and keep the coder role's work across the respawn with a new `attempt_started`. The respawned coder's `DOT_AGENT_DECK_WORK_ID` must still be the orchestration's work id.
#[spec("journal/launch/003")]
#[test]
fn journal_launch_003_orchestration_delegation_and_report_are_recorded() {
    let daemon = inbox::daemon_unbound(&[]);
    let state = journal::default_state_dir(&daemon);
    let (orch, old) = route::team(&daemon, true);
    inbox::status(&daemon, &old, "idle", "claude_code");

    let opened = journal::wait_for(&state, "an `opened` launch=orchestration", |l| {
        journal::find(l, "opened", |o| {
            o["launch"] == "orchestration" && o["refs"]["orchestration_id"] == "route-fixture"
        })
        .cloned()
    });
    assert_envelope(&opened);
    let orch_work = str_of(&opened, "work").to_string();
    assert_eq!(opened["refs"]["orchestration"], "route", "{opened}");
    assert_eq!(
        opened["workspace"], "~",
        "the orchestration cwd is the home directory, written `~`: {opened}"
    );
    for (pane, role) in [(&orch, "orchestrator"), (&old, "coder")] {
        let attempt = journal::wait_for(&state, &format!("the {role}'s `attempt_started`"), |l| {
            journal::attempt_of(l, &pane.agent).cloned()
        });
        assert_eq!(
            attempt["work"],
            orch_work.as_str(),
            "a role pane is an attempt of the orchestration's work: {attempt}"
        );
        assert_eq!(attempt["role"], role, "{attempt}");
        assert_eq!(
            work_env(pane),
            orch_work,
            "the {role}'s DOT_AGENT_DECK_WORK_ID must be the orchestration work id"
        );
    }

    let task_id = route::delegate(&daemon, &orch, route::TASK, false);
    let successor = std::cell::RefCell::new(None);
    assert!(
        common::wait_until(inbox::WAIT, || {
            *successor.borrow_mut() = daemon
                .agent_records()
                .into_iter()
                .find(|r| r.pane_id_env.as_deref() == Some(&old.id) && r.id != old.agent)
                .map(|r| r.id);
            successor.borrow().is_some()
        }),
        "precondition: a clear delegate replaces the worker"
    );
    let new = route::observe(&daemon, &successor.into_inner().unwrap());
    route::count(&daemon, &new, 1);

    let delegation = journal::wait_for(&state, "an `opened` launch=delegation", |l| {
        journal::find(l, "opened", |o| {
            o["launch"] == "delegation" && o["refs"]["task_id"] == task_id.as_str()
        })
        .cloned()
    });
    let deleg_work = str_of(&delegation, "work").to_string();
    assert_eq!(delegation["parent"], orch_work.as_str(), "{delegation}");
    assert_eq!(delegation["refs"]["role"], "coder", "{delegation}");

    let respawned = journal::wait_for(
        &state,
        "the respawned coder's `attempt_started` on the orchestration work",
        |l| {
            journal::find(l, "attempt_started", |a| {
                a["work"] == orch_work.as_str()
                    && a["agent"].as_u64().map(|n| n.to_string()) == Some(new.agent.clone())
            })
            .cloned()
        },
    );
    assert_eq!(respawned["pane"], old.id.as_str(), "{respawned}");
    assert_eq!(respawned["role"], "coder", "{respawned}");
    let old_attempt = format!("{}/{}", str_of(&opened, "run"), old.agent);
    journal::wait_for(
        &state,
        "the replaced coder generation's `attempt_ended`",
        |l| {
            journal::find(l, "attempt_ended", |e| {
                e["attempt"] == old_attempt.as_str() && e["how"] == "replaced"
            })
            .cloned()
        },
    );
    journal::wait_for(
        &state,
        "the delegation's attempt on the bound worker",
        |l| {
            journal::find(l, "attempt_started", |a| {
                a["work"] == deleg_work.as_str()
                    && a["agent"].as_u64().map(|n| n.to_string()) == Some(new.agent.clone())
            })
            .cloned()
        },
    );
    assert_eq!(
        work_env(&new),
        orch_work,
        "the respawned coder's DOT_AGENT_DECK_WORK_ID must be the orchestration's work id \
         (its role's work), not the delegation's {deleg_work}"
    );

    route::report(&daemon, &new, &task_id, route::REPORT);
    let reported = journal::wait_for(&state, "the delegation's `reported`", |l| {
        journal::find(l, "reported", |r| r["work"] == deleg_work.as_str()).cloned()
    });
    assert_eq!(reported["task_id"], task_id.as_str(), "{reported}");
    assert_eq!(reported["destination"], "inbox", "{reported}");
    assert_eq!(reported["to"], orch_work.as_str(), "{reported}");
    assert!(
        reported["report_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{reported}"
    );
    journal::wait_for(&state, "the delegation's `closed how=task_done`", |l| {
        journal::find(l, "closed", |c| {
            c["work"] == deleg_work.as_str() && c["how"] == "task_done"
        })
        .cloned()
    });
    let text = journal::dump(&state);
    assert!(
        !text.contains("ROUTE-REPORT-b3d8") && !text.contains("ROUTE-TASK-a971"),
        "the journal must hold no task or report text:\n{text}"
    );
}

/// Scenario: Fire a plain scheduled task whose command is the routing stand-in. The journal must open a `launch=schedule_fire` work naming the schedule, the spawned pane must be an attempt of it, and the stand-in's `DOT_AGENT_DECK_WORK_ID` must name it.
#[spec("journal/launch/004")]
#[test]
fn journal_launch_004_a_scheduled_fire_opens_a_work_per_fire() {
    let scratch = common::harness_tempdir().unwrap();
    let work_dir = scratch.path().join("journal-fire");
    std::fs::create_dir(&work_dir).unwrap();
    let cfg = route::schedule("journal-fire", &work_dir, &route::command(&work_dir));
    let daemon = common::spawn_daemon_serve_with_env(Some(&cfg), "0", &[]);
    let state = journal::default_state_dir(&daemon);
    daemon.run_now("journal-fire").unwrap();
    let records = daemon.wait_for_agent_count(1, inbox::WAIT);
    let pane = route::observe(&daemon, &records[0].id);

    let opened = journal::wait_for(&state, "an `opened` launch=schedule_fire", |l| {
        journal::find(l, "opened", |o| {
            o["launch"] == "schedule_fire" && o["refs"]["schedule"] == "journal-fire"
        })
        .cloned()
    });
    assert_envelope(&opened);
    let work = str_of(&opened, "work").to_string();
    let attempt = journal::wait_for(&state, "the fired pane's `attempt_started`", |l| {
        journal::attempt_of(l, &pane.agent).cloned()
    });
    assert_eq!(attempt["work"], work.as_str(), "{attempt}");
    assert_eq!(
        work_env(&pane),
        work,
        "the fired agent's DOT_AGENT_DECK_WORK_ID"
    );
}

/// Scenario: A routing stand-in pane in a one-commit repository that defines a one-role orchestration runs the real `dot-agent-deck dispatch --orchestration=journal-team`. The journal must open a `launch=dispatch` work parented on the dispatcher pane's work and a `launch=orchestration` work named `journal-team` whose parent is that dispatch work; the orchestrator role's generation is an attempt of the orchestration work and its `DOT_AGENT_DECK_WORK_ID` names it.
#[spec("journal/launch/006")]
#[test]
fn journal_launch_006_a_dispatched_orchestration_is_a_child_of_its_dispatch() {
    const UNIT: &str = "journal-team-unit";
    let daemon = inbox::daemon_unbound(&[]);
    let state = journal::default_state_dir(&daemon);
    let repo = daemon.home.join("journal-team-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let cmd = route::command(&repo);
    std::fs::write(
        repo.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\nname = 'journal-team'\n[[orchestrations.roles]]\nname = 'orchestrator'\nstart = true\ncommand = {}\n",
            serde_json::to_string(&cmd).unwrap()
        ),
    )
    .unwrap();
    journal::init_repo(&repo);
    let caller = route::start(&daemon, "journal-caller", json!({"cwd": repo}));
    let orchestrator = dispatch_as(
        &daemon,
        &caller,
        &repo,
        UNIT,
        "--orchestration=journal-team",
    );
    let worktree = cwd_of(&daemon, &orchestrator.agent);

    let caller_work = journal::wait_for(&state, "the dispatcher pane's `attempt_started`", |l| {
        journal::attempt_of(l, &caller.agent).map(|a| str_of(a, "work").to_string())
    });
    let dispatch = dispatch_opened(&state, UNIT);
    assert_envelope(&dispatch);
    let dispatch_work = str_of(&dispatch, "work").to_string();
    assert_eq!(
        dispatch["parent"],
        caller_work.as_str(),
        "the dispatch's parent is the dispatcher pane's work: {dispatch}"
    );
    assert!(
        journal::workspace_is(&dispatch, &daemon.home, &worktree),
        "the dispatch's workspace must be the unit's worktree {:?}: {dispatch}",
        journal::workspace_spellings(&daemon.home, &worktree)
    );

    let orch = journal::wait_for(
        &state,
        "an `opened` launch=orchestration for journal-team",
        |l| {
            journal::find(l, "opened", |o| {
                o["launch"] == "orchestration" && o["refs"]["orchestration"] == "journal-team"
            })
            .cloned()
        },
    );
    assert_envelope(&orch);
    let orch_work = str_of(&orch, "work").to_string();
    assert_ne!(orch_work, dispatch_work, "two works: {orch}");
    assert_eq!(
        orch["parent"],
        dispatch_work.as_str(),
        "an orchestration started by a dispatch is a child of the dispatch's work: {orch}"
    );
    assert!(
        orch["refs"]["orchestration_id"].is_string(),
        "refs.orchestration_id: {orch}"
    );

    let attempt = journal::wait_for(&state, "the orchestrator role's `attempt_started`", |l| {
        journal::attempt_of(l, &orchestrator.agent).cloned()
    });
    assert_eq!(
        attempt["work"],
        orch_work.as_str(),
        "a role generation is an attempt of the orchestration's work: {attempt}"
    );
    assert_eq!(attempt["role"], "orchestrator", "{attempt}");
    assert_eq!(
        work_env(&orchestrator),
        orch_work,
        "the orchestrator's DOT_AGENT_DECK_WORK_ID must be the orchestration work id"
    );
}

/// Start a pane and a dispatched unit under one daemon on `state`, end that
/// daemon with `signal` sent to its own pid, and start a second on the same
/// state directory. Returns the parts the two restart tests assert on.
struct Restarted {
    second: common::DaemonProc,
    state: PathBuf,
    dead: PathBuf,
    dead_bytes: Vec<u8>,
    attempts: Vec<String>,
    _first: common::DaemonProc,
    _scratch: tempfile::TempDir,
}

fn restart_after(signal: libc::c_int, torn_tail: bool) -> Restarted {
    const UNIT: &str = "journal-restart";
    let scratch = common::harness_tempdir().unwrap();
    let state = scratch.path().join("state");
    let state_env = state.to_string_lossy().into_owned();
    let mut first = inbox::daemon_unbound(&[("DOT_AGENT_DECK_STATE_DIR", &state_env)]);
    let repo = first.home.join("journal-restart-repo");
    journal::init_repo(&repo);
    let caller = route::start(&first, "journal-caller", json!({"cwd": repo}));
    let unit = dispatch_single(&first, &caller, &repo, UNIT);
    let worktree = cwd_of(&first, &unit.agent);

    let (caller_work, attempts) = journal::wait_for(&state, "both launches' attempts", |l| {
        let caller_attempt = journal::attempt_of(l, &caller.agent)?;
        let unit_attempt = journal::attempt_of(l, &unit.agent)?;
        Some((
            str_of(caller_attempt, "work").to_string(),
            vec![
                str_of(caller_attempt, "attempt").to_string(),
                str_of(unit_attempt, "attempt").to_string(),
            ],
        ))
    });
    let segments = journal::segments(&state);
    assert_eq!(journal::runs(&state).len(), 1, "one live run: {segments:?}");
    assert_eq!(
        segments.len(),
        1,
        "a run this small never rotates: {segments:?}"
    );
    let dead = segments[0].clone();

    // The first daemon's own pid, never a pattern (CLAUDE.md rule 12).
    // SAFETY: kill(2) on a pid this test spawned and still holds.
    assert_eq!(unsafe { libc::kill(first.pid() as libc::pid_t, signal) }, 0);
    assert!(
        first.wait_for_exit(common::load_scaled(Duration::from_secs(20))),
        "the first daemon must exit on signal {signal}"
    );
    drop((caller, unit));
    if torn_tail {
        use std::io::Write;
        let torn = format!(
            "{{\"v\":1,\"seq\":999,\"ts_ms\":1,\"run\":\"{}\",\"kind\":\"attempt_ended\",\"work\":\"{caller_work}\",\"attempt\":\"{}\",\"how\":\"exi",
            journal::segment_run(&dead),
            attempts[0]
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&dead)
            .unwrap()
            .write_all(torn.as_bytes())
            .unwrap();
    }
    let dead_bytes = std::fs::read(&dead).unwrap();

    let mut second = inbox::daemon_unbound(&[("DOT_AGENT_DECK_STATE_DIR", &state_env)]);
    assert!(second.is_alive_public(), "the second daemon must start");

    // What the dead run recorded is still readable after the restart.
    let lines = journal::lines(&state);
    let pane_open = journal::find(&lines, "opened", |o| {
        o["work"] == caller_work.as_str() && o["launch"] == "pane"
    })
    .unwrap_or_else(|| panic!("the pane's `opened` survives:\n{}", journal::dump(&state)));
    assert!(
        journal::workspace_is(pane_open, &first.home, &repo),
        "{pane_open}"
    );
    let dispatch_open = journal::find(&lines, "opened", |o| o["launch"] == "dispatch")
        .unwrap_or_else(|| {
            panic!(
                "the dispatch's `opened` survives:\n{}",
                journal::dump(&state)
            )
        });
    assert_eq!(
        dispatch_open["parent"],
        caller_work.as_str(),
        "{dispatch_open}"
    );
    assert_eq!(
        dispatch_open["refs"]["branch"],
        format!("agent/dispatch-{UNIT}"),
        "{dispatch_open}"
    );
    assert!(
        dispatch_open["refs"]["unit_id"].is_string(),
        "{dispatch_open}"
    );
    assert!(
        journal::workspace_is(dispatch_open, &first.home, &worktree),
        "{dispatch_open}"
    );

    Restarted {
        second,
        state,
        dead,
        dead_bytes,
        attempts,
        _first: first,
        _scratch: scratch,
    }
}

/// The new run's own segment, once it exists: one whose run is not the dead
/// segment's (a rotated `run-<dead>.<n>.jsonl` is still the dead run).
fn new_segment(state: &Path, dead: &Path) -> PathBuf {
    let found = std::cell::RefCell::new(None);
    let budget = journal::wait();
    let dead_run = journal::segment_run(dead);
    common::wait_until(budget, || {
        *found.borrow_mut() = journal::segments(state)
            .into_iter()
            .find(|s| journal::segment_run(s) != dead_run);
        found.borrow().is_some()
    });
    found.into_inner().unwrap_or_else(|| {
        panic!(
            "the second daemon never opened its own run segment within {budget:?}.\n{}",
            journal::dump(state)
        )
    })
}

/// Every one of `attempts` has an `attempt_ended how=interrupted`; returns
/// those lines.
fn interrupted(state: &Path, attempts: &[String]) -> Vec<Value> {
    journal::wait_for(
        state,
        "`attempt_ended how=interrupted` for every open attempt",
        |l| {
            attempts
                .iter()
                .map(|attempt| {
                    journal::find(l, "attempt_ended", |e| {
                        e["attempt"] == attempt.as_str() && e["how"] == "interrupted"
                    })
                    .cloned()
                })
                .collect::<Option<Vec<_>>>()
        },
    )
}

/// The dead segment was never rewritten: it is byte for byte what the dead
/// run (and the test) left, or compaction folded it away and a
/// `compacted-*.jsonl` carries every one of its complete lines byte for byte
/// (its torn tail, if any, may be dropped).
fn assert_dead_segment_untouched(r: &Restarted) {
    match std::fs::read(&r.dead) {
        Ok(bytes) => assert!(
            bytes == r.dead_bytes,
            "the dead segment {} must not be rewritten.\nbefore:\n{}\nafter:\n{}",
            r.dead.display(),
            String::from_utf8_lossy(&r.dead_bytes),
            String::from_utf8_lossy(&bytes)
        ),
        Err(_) => {
            let compacted = journal::compacted_bytes(&r.state);
            for line in journal::complete_lines(&r.dead_bytes) {
                assert!(
                    journal::holds_line(&compacted, &line),
                    "the dead segment was folded away, but no compacted file carries its \
                     line byte for byte:\n{}\njournal:\n{}",
                    String::from_utf8_lossy(&line),
                    journal::dump(&r.state)
                );
            }
        }
    }
}

/// Scenario: Start a pane and dispatch a single unit from it, SIGTERM that daemon by its own pid, and start a new daemon on the same state directory. The new daemon must create its own `run-<id>.jsonl` even with nothing to write, both launches must still read back with their parent, refs and workspace, and every attempt that was live must end `how=interrupted` with cause `daemon_shutdown` or `daemon_gone`. The dead segment must be untouched, or folded into a `compacted-*.jsonl` that carries its lines byte for byte.
#[spec("journal/restart/001")]
#[test]
fn journal_restart_001_sigterm_leaves_every_live_attempt_interrupted() {
    let mut r = restart_after(libc::SIGTERM, false);
    // A new run opens its own segment even when it has nothing to write: the
    // dying daemon may have written every end itself (`daemon_shutdown`).
    let new = new_segment(&r.state, &r.dead);
    let ends = interrupted(&r.state, &r.attempts);
    for end in &ends {
        assert!(
            matches!(
                end["cause"].as_str(),
                Some("daemon_shutdown" | "daemon_gone")
            ),
            "an interrupted end names its cause: {end}"
        );
    }
    let run = journal::segment_run(&new);
    assert_ne!(run, journal::segment_run(&r.dead));
    for line in journal::file_lines(&new) {
        assert_eq!(
            line["run"],
            run.as_str(),
            "a segment holds only its own run: {line}"
        );
    }
    assert_dead_segment_untouched(&r);
    assert!(r.second.is_alive_public());
}

/// Scenario: Start a pane and dispatch a single unit from it, SIGKILL that daemon by its own pid, and append a torn half line with no newline to its segment. A new daemon on the same state directory must start normally, write `how=interrupted cause=daemon_gone` for both open attempts into its own segment, and leave the dead segment byte for byte as it was (or folded into a `compacted-*.jsonl` carrying its complete lines byte for byte).
#[spec("journal/restart/002")]
#[test]
fn journal_restart_002_sigkill_with_a_torn_tail_recovers_without_rewriting() {
    let mut r = restart_after(libc::SIGKILL, true);
    let ends = interrupted(&r.state, &r.attempts);
    let new = new_segment(&r.state, &r.dead);
    let run = journal::segment_run(&new);
    for end in &ends {
        assert_eq!(end["cause"], "daemon_gone", "{end}");
        assert_eq!(
            end["run"],
            run.as_str(),
            "recovery writes into the new run: {end}"
        );
    }
    assert_dead_segment_untouched(&r);
    assert!(
        r.second.is_alive_public() && r.second.agent_records().is_empty(),
        "the new daemon serves normally after recovery"
    );
}

/// Scenario: Before a daemon starts, plant a dead run segment holding a v1 `opened` and `attempt_started`, a `"v":2` line and a v1 line of an unknown kind. The daemon must start normally, write `how=interrupted cause=daemon_gone` for the planted attempt, and keep the v2 and unknown-kind lines byte for byte.
#[spec("journal/compat/001")]
#[test]
fn journal_compat_001_newer_and_unknown_lines_are_skipped_and_kept() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    const DEAD_RUN: &str = "0dead00000000000000000000000c0a1";
    const WORK: &str = "w-plantdck-00000000000000a1";
    const ATTEMPT: &str = "0dead00000000000000000000000c0a1/41";
    let scratch = common::harness_tempdir().unwrap();
    let state = scratch.path().join("state");
    let dir = journal::dir(&state);
    std::fs::create_dir_all(&dir).unwrap();
    for d in [&state, &dir] {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let opened = format!(
        r#"{{"v":1,"seq":1,"ts_ms":{now},"run":"{DEAD_RUN}","kind":"opened","work":"{WORK}","launch":"pane","refs":{{"pane":"planted-pane"}},"workspace":"/planted"}}"#
    );
    let started = format!(
        r#"{{"v":1,"seq":2,"ts_ms":{now},"run":"{DEAD_RUN}","kind":"attempt_started","work":"{WORK}","attempt":"{ATTEMPT}","pane":"planted-pane","agent":41}}"#
    );
    let newer = format!(
        r#"{{"v":2,"seq":3,"ts_ms":{now},"run":"{DEAD_RUN}","kind":"attempt_ended","work":"{WORK}","attempt":"{ATTEMPT}","how":"exited","code":0,"future":{{"x":[1,2]}}}}"#
    );
    let unknown = format!(
        r#"{{"v":1,"seq":4,"ts_ms":{now},"run":"{DEAD_RUN}","kind":"teleported","work":"{WORK}","attempt":"{ATTEMPT}","how":"exited"}}"#
    );
    let planted = format!("{opened}\n{started}\n{newer}\n{unknown}\n");
    let segment = dir.join(format!("run-{DEAD_RUN}.jsonl"));
    {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&segment)
            .unwrap()
            .write_all(planted.as_bytes())
            .unwrap();
    }

    let state_env = state.to_string_lossy().into_owned();
    let mut daemon = inbox::daemon_unbound(&[("DOT_AGENT_DECK_STATE_DIR", &state_env)]);
    let end = journal::wait_for(
        &state,
        "`attempt_ended how=interrupted` for the planted attempt",
        |l| {
            journal::find(l, "attempt_ended", |e| {
                e["attempt"] == ATTEMPT && e["how"] == "interrupted" && e["run"] != DEAD_RUN
            })
            .cloned()
        },
    );
    assert_eq!(end["work"], WORK, "{end}");
    assert_eq!(end["cause"], "daemon_gone", "{end}");
    assert_eq!(end["v"], 1, "{end}");

    let all: Vec<u8> = journal::files(&state)
        .iter()
        .flat_map(|f| std::fs::read(f).unwrap_or_default())
        .collect();
    for (what, line) in [("the v2 line", &newer), ("the unknown-kind line", &unknown)] {
        let needle = format!("{line}\n");
        assert!(
            all.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "{what} must be kept byte for byte:\n{}",
            journal::dump(&state)
        );
    }
    match std::fs::read(&segment) {
        Ok(bytes) => assert_eq!(
            String::from_utf8_lossy(&bytes),
            planted,
            "the dead segment is never rewritten"
        ),
        Err(_) => {
            let compacted = journal::compacted_bytes(&state);
            for line in journal::complete_lines(planted.as_bytes()) {
                assert!(
                    journal::holds_line(&compacted, &line),
                    "the planted segment was folded away without its line:\n{}\njournal:\n{}",
                    String::from_utf8_lossy(&line),
                    journal::dump(&state)
                );
            }
        }
    }
    assert!(
        daemon.is_alive_public() && daemon.agent_records().is_empty(),
        "the daemon starts and serves normally"
    );
}
