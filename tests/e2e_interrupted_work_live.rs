#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! L2 REAL-scenario coverage for the interrupted work list (PRD #1790 stage
//! 2, CLAUDE.md rule 4): a dispatched unit whose agent is a REAL interactive
//! Claude Code on Haiku, stopped by a daemon restart, then found and
//! reclaimed the way a person does it — the CLI's `interrupted list`, and a
//! fresh TUI's footer, `I` list and reclaim.
//!
//! The deterministic half of this surface is `tests/e2e_interrupted_work.rs`
//! (lane 1), whose units are routing stand-ins. What a stand-in cannot show is
//! that the work a real agent leaves behind — its worktree after a live
//! session, with whatever the agent wrote there — is what the list names and
//! what reclaim can remove. The dispatcher here is still a stand-in pane: it
//! only runs `dot-agent-deck dispatch`, which a person or a dispatcher agent
//! runs the same way, and it costs no tokens.
//!
//! The restart is lane 1's: SIGTERM to the daemon's own pid (never a
//! pattern, CLAUDE.md rule 12) and a second `daemon serve` on the same state
//! directory. The agent's first-run onboarding and the trust of the worktree
//! and of the hold's directory are seeded into the daemon's HOME before the
//! dispatch, as
//! `scheduler/dispatch/013` seeds them, with the deck's Claude Code hooks,
//! so the unattended `claude` takes its task from the deck with no keystroke.
//!
//! Both daemons bind inbox components normally (`inbox::daemon`, not
//! `daemon_unbound`): the unit is a real agent whose hook shells the daemon
//! resolves to that agent, which the test switch skips, so that switch could
//! refuse the agent's own waiter and stall its task. Each daemon logs to a
//! file in the test root, which a failure message quotes.
//!
//! The agent's command ends by reading a FIFO ([`hold::FifoHold`]) that the
//! test creates before the dispatch, at an absolute path inside this run's own
//! test root, and never writes to, so the command blocks until the test lets
//! it go. The test waits until that exact command is running among the
//! daemon's descendants and checks it again just before the restart; after
//! it, the journal must say the unit's attempt ended interrupted and that the
//! unit reported nothing. Elapsed time proves neither. Nothing in the test
//! signals the held command: dropping the hold closes the FIFO's last writer,
//! so a `cat` the restart left running reads end-of-file and exits.
//!
//! Cost: one short Haiku turn (one Bash command). Lane 2: it reaches a real
//! agent, so it runs on a developer's machine and never in CI.

dot_agent_deck_test_harness::link!();
#[path = "support/fifo_hold.rs"]
mod hold;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_view.rs"]
mod inbox_view;
#[path = "support/work_journal.rs"]
mod journal;
#[path = "support/inbox_route.rs"]
mod route;

use dot_agent_deck::inbox::StateName;
use dot_agent_deck::interrupted_work::{
    INTERRUPTED_WORK_TITLE, RECLAIM_EXPLANATION, RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE,
    interrupted_footer,
};
use serde_json::{Value, json};
use spec::spec;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// The unit's name, and so its branch `agent/dispatch-<UNIT>` and worktree
/// `<repo>-dispatch-<UNIT>`.
const UNIT: &str = "interrupted-live";

/// The file the agent writes, under the worktree's git-ignored
/// `.dot-agent-deck/` so writing it leaves the tree clean.
const SENTINEL: &str = "interrupted-live-sentinel-7c41.txt";

/// The file name of the FIFO the agent's command reads last, in a directory
/// of this run's own test root; its absolute path is what identifies it.
const HOLD: &str = "interrupted-live-hold-7c41.fifo";

/// Every process now running, as `(pid, parent pid, argv)` from `ps`, which
/// macOS and Linux both have. `argv` is split on spaces, which is enough to
/// find a command whose arguments hold none.
fn processes() -> Vec<(u32, u32, Vec<String>)> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,args="])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let ppid = words.next()?.parse().ok()?;
            Some((pid, ppid, words.map(str::to_string).collect()))
        })
        .collect()
}

/// The processes under `ancestor` (its descendants, not itself), as
/// `(pid, argv)`.
fn descendants(ancestor: u32) -> Vec<(u32, Vec<String>)> {
    let all = processes();
    let parent: std::collections::HashMap<u32, u32> =
        all.iter().map(|(pid, ppid, _)| (*pid, *ppid)).collect();
    // Up the parent chain until `ancestor`, the root, or a pid seen before.
    let under = |mut pid: u32| {
        let mut seen = std::collections::HashSet::new();
        while seen.insert(pid) {
            match parent.get(&pid) {
                Some(p) if *p == ancestor => return true,
                Some(p) if *p > 1 => pid = *p,
                _ => return false,
            }
        }
        false
    };
    all.into_iter()
        .filter(|(pid, _, _)| under(*pid))
        .map(|(pid, _, argv)| (pid, argv))
        .collect()
}

/// The processes under `ancestor` running `cat` of exactly this run's FIFO.
fn holding_commands(ancestor: u32, hold: &hold::FifoHold) -> Vec<(u32, Vec<String>)> {
    descendants(ancestor)
        .into_iter()
        .filter(|(_, argv)| hold.held_by(argv))
        .collect()
}

/// Every process under `ancestor` with its argv, for a failure message.
fn process_tree(ancestor: u32) -> String {
    descendants(ancestor)
        .into_iter()
        .map(|(pid, argv)| format!("{pid}: {}", argv.join(" ")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The real CLI as a person at a terminal runs it against `daemon`: no pane
/// identity, no terminal on stdin.
fn person(daemon: &common::DaemonProc, repo: &Path, args: &[&str]) -> Output {
    std::process::Command::new(common::contained_deck_binary())
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &daemon.home)
        .env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &daemon.attach_socket)
        .stdin(std::process::Stdio::null())
        .current_dir(repo)
        .output()
        .expect("run the deck CLI")
}

fn text(out: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn branch_listed(repo: &Path, branch: &str) -> bool {
    let out = common::fixture_git(repo, repo)
        .args(["branch", "--list", branch])
        .output()
        .expect("git available");
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

fn porcelain(dir: &Path) -> String {
    let out = common::fixture_git(dir, dir)
        .args(["status", "--porcelain"])
        .output()
        .expect("git available");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `agent`'s pane as its screen now reads, for a failure message.
fn pane_screen(daemon: &common::DaemonProc, agent: &str) -> String {
    let rows = std::num::NonZeroU16::new(40).expect("non-zero");
    let cols = std::num::NonZeroU16::new(160).expect("non-zero");
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(&common::pane_snapshot_on(&daemon.attach_socket, agent));
    parser.screen().contents()
}

/// The last `lines` lines of the daemon log at `path`, for a failure message.
fn log_tail(path: &Path, lines: usize) -> String {
    let log = std::fs::read_to_string(path).unwrap_or_else(|e| format!("<no log: {e}>"));
    let all: Vec<&str> = log.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Both spellings of `path` a `claude` started there may key its trust by:
/// as written, and with its parent resolved through symlinks.
fn trust_spellings(path: &Path) -> Vec<String> {
    let mut spellings = vec![path.to_string_lossy().into_owned()];
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = parent.canonicalize()
    {
        let canonical = parent.join(name).to_string_lossy().into_owned();
        if !spellings.contains(&canonical) {
            spellings.push(canonical);
        }
    }
    spellings
}

/// Scenario: A stand-in dispatcher pane dispatches a `--single` unit whose agent is a real interactive Claude Code on Haiku; the deck delivers the task, which the agent acknowledges, and the agent's one command writes a uniquely named sentinel under the worktree's ignored `.dot-agent-deck/` and then blocks reading a FIFO the test created before the dispatch and never writes, which the test waits to see running under the daemon and checks again just before it restarts the daemon on the same state directory.
/// After the restart the journal says the unit's attempt ended interrupted with no report, the person's `interrupted list` names the unit with its branch and worktree, the worktree is clean, and a TUI attached to the new daemon shows the "interrupted items" footer, lists the unit under "Interrupted work" after `I`, and reclaims it with `r` and `y`.
/// The worktree is gone, the branch is kept, the footer counts one item, and the CLI no longer lists the unit.
#[spec("journal/interrupted/028")]
#[test]
fn interrupted_028_a_real_agents_unit_is_listed_and_reclaimed_after_a_restart() {
    // Decision 26 runtime-skip: a missing CLI or credential is environmental;
    // DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 turns it into a failure.
    skip_unless!(common::check_claude_available());

    let scratch = common::harness_tempdir().expect("scratch tempdir");
    let state = scratch.path().join("state");
    let state_env = state.to_string_lossy().into_owned();
    let path = common::path_with_binary_dir();
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty());
    let mut env: Vec<(&str, &str)> = vec![
        ("DOT_AGENT_DECK_STATE_DIR", &state_env),
        // The agent may run the deck's own CLI, as a dispatched agent does.
        ("PATH", &path),
        // A real agent's boot and turn outlast the harness's 300 s default.
        ("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "900"),
        // Claude Code's Bash tool stops a command after two minutes by
        // default; the held command must outlast the checks before the stop.
        ("BASH_DEFAULT_TIMEOUT_MS", "600000"),
        ("BASH_MAX_TIMEOUT_MS", "600000"),
    ];
    if let Some(key) = api_key.as_deref() {
        env.push(("ANTHROPIC_API_KEY", key));
    }
    let first_log = scratch.path().join("deck-first.log");
    let first_log_env = first_log.to_string_lossy().into_owned();
    let mut first_env = env.clone();
    first_env.push(("DOT_AGENT_DECK_LOG", &first_log_env));
    let mut live = inbox::daemon(&first_env);

    // A one-commit repository that ignores the deck's `.dot-agent-deck/` and
    // the stand-in's files, so the unit's worktree reads clean until its
    // agent changes a tracked or unignored file.
    let repo = scratch.path().join("interrupted-live-repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "interrupted work, live\n").unwrap();
    journal::init_repo(&repo);
    std::fs::write(
        repo.join(".git").join("info").join("exclude"),
        "/.dot-agent-deck/\n.route-*\n.test-capability-*\n",
    )
    .unwrap();

    // The unit's worktree is a sibling of the repository, inside `scratch`.
    let worktree = scratch
        .path()
        .join(format!("interrupted-live-repo-dispatch-{UNIT}"));
    let branch = format!("agent/dispatch-{UNIT}");
    // The hold's directory, which the agent is given with `--add-dir`: Claude
    // Code asks whether to trust an added directory as it asks for its working
    // one, so both are seeded as trusted.
    let hold_dir = scratch.path().join("hold");
    let mut trusted = trust_spellings(&worktree);
    trusted.extend(trust_spellings(&hold_dir));
    common::seed_claude_worker_home(&live.home, &trusted).expect(
        "seed the agent's credentials, onboarding and the trust of the worktree and the hold's directory",
    );
    // The deck's Claude Code hooks, in the HOME the agent runs under: the
    // credential import strips the host's hooks, and the deck learns that the
    // agent is ready for its task from them (as `e2e_pi_orchestrator.rs` does).
    let installed = std::process::Command::new(common::contained_deck_binary())
        .args(["hooks", "install", "--agent", "claude-code"])
        .current_dir(&repo)
        .env("HOME", &live.home)
        .env("CLAUDE_CONFIG_DIR", live.home.join(".claude"))
        .env("DOT_AGENT_DECK_LOG", live.home.join("hook-install.log"))
        .env("PATH", &path)
        .output()
        .expect("install the deck's Claude Code hooks");
    assert!(
        installed.status.success(),
        "hook install failed: {}",
        text(&installed)
    );

    // The hold, before the dispatch, so it exists whenever the agent reaches
    // its command: in this run's own root, outside the worktree, with the
    // agent given access to its directory. Dropping it, on a pass or a
    // failure, releases the command (`support/fifo_hold.rs`).
    std::fs::create_dir_all(&hold_dir).expect("the hold's directory");
    let hold = hold::FifoHold::create(&hold_dir.join(HOLD));

    let caller = route::start(&live, "interrupted-live-caller", json!({"cwd": repo}));
    inbox::status(&live, &caller, "session_start", "codex");
    inbox::status(&live, &caller, "thinking", "codex");
    // The command ends reading the FIFO, which nothing writes, so the agent
    // is still in it, and has not reported, when the restart stops it: the
    // work is interrupted, not done. The command is on a line of its own, so
    // no word of the prose after it can be read as one of its arguments.
    let hold_path = hold.path().display();
    let task = format!(
        "Use the Bash tool to run exactly this one command now, in the foreground, as one \
         command, and wait for it:\n\n\
         `mkdir -p .dot-agent-deck && echo done > .dot-agent-deck/{SENTINEL} && cat {hold_path}`\n\n\
         Do not run any other command, do not create or change any other file, and do not \
         commit."
    );
    // The deck delivers the task: no first prompt on the command line.
    let agent_command = format!(
        "claude --ax-screen-reader --model {HAIKU_MODEL} --add-dir {} --allowedTools Bash Read Write",
        hold_dir.display()
    );
    route::cli(
        &live,
        &caller,
        &repo,
        &[
            "dispatch",
            UNIT,
            "--single",
            "--command",
            &agent_command,
            "--task",
            &task,
        ],
    );

    // The unit spawns in its worktree.
    let suffix = format!("-dispatch-{UNIT}");
    let spawned = std::cell::RefCell::new(None);
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(60)), || {
            *spawned.borrow_mut() = common::agent_records_on(&live.attach_socket)
                .into_iter()
                .find(|r| {
                    r.id != caller.agent && r.cwd.as_deref().is_some_and(|c| c.ends_with(&suffix))
                })
                .map(|r| r.id);
            spawned.borrow().is_some()
        }),
        "the dispatch never spawned its unit: {:?}",
        common::agent_records_on(&live.attach_socket)
    );
    let unit_agent = spawned.into_inner().unwrap();
    // The unit's pane id, which its inbox is keyed by.
    let unit_pane = common::agent_records_on(&live.attach_socket)
        .into_iter()
        .find(|r| r.id == unit_agent)
        .and_then(|r| r.pane_id_env)
        .expect("the unit's pane id");

    // The real agent does its one command.
    let sentinel = worktree.join(".dot-agent-deck").join(SENTINEL);
    let began = std::time::Instant::now();
    const SENTINEL_WAIT: Duration = Duration::from_secs(240);
    assert!(
        common::wait_until(SENTINEL_WAIT, || sentinel.is_file()),
        "the real Haiku agent never wrote {} within {}s.\nThe agent's pane:\n{}\nIts inbox:\n{:?}\nDaemon log ({}):\n{}",
        sentinel.display(),
        SENTINEL_WAIT.as_secs(),
        pane_screen(&live, &unit_agent),
        inbox_view::messages(&live.attach_socket, &unit_pane),
        first_log.display(),
        log_tail(&first_log, 80)
    );
    eprintln!(
        "timing: the agent wrote the sentinel {:?} after the dispatch",
        began.elapsed()
    );
    // The deck's own delivery took the task to the agent: its seed, the
    // message naming the sentinel, was handed over and acknowledged.
    let seed_acknowledged = || {
        inbox_view::messages(&live.attach_socket, &unit_pane)
            .into_iter()
            .any(|message| {
                message.state == StateName::Acknowledged
                    && message
                        .text
                        .as_deref()
                        .is_some_and(|text| text.contains(SENTINEL))
            })
    };
    assert!(
        common::wait_until(
            common::load_scaled(Duration::from_secs(30)),
            seed_acknowledged
        ),
        "the dispatched task was not acknowledged by the unit: {:?}\nDaemon log ({}):\n{}",
        inbox_view::messages(&live.attach_socket, &unit_pane),
        first_log.display(),
        log_tail(&first_log, 80)
    );
    let unit_attempt = journal::wait_for(&state, "both launches' attempts", |lines| {
        let unit = journal::attempt_of(lines, &unit_agent)?;
        journal::attempt_of(lines, &caller.agent)?;
        Some((unit["work"].clone(), unit["attempt"].clone()))
    });

    // The sentinel is written before `cat` starts, so wait, bounded, until
    // the agent's `cat` of this run's FIFO is running among the daemon's
    // descendants.
    let held_wait = common::load_scaled(Duration::from_secs(30));
    assert!(
        common::wait_until(held_wait, || !holding_commands(live.pid(), &hold)
            .is_empty()),
        "the agent's command never reached its `cat` of {} under the daemon (pid {}) within {}s.\nIts process tree:\n{}\nThe agent's pane:\n{}",
        hold.path().display(),
        live.pid(),
        held_wait.as_secs(),
        process_tree(live.pid()),
        pane_screen(&live, &unit_agent)
    );
    // Just before the stop, the agent's command is still running.
    let held = holding_commands(live.pid(), &hold);
    for (pid, argv) in &held {
        eprintln!("held: pid {pid} runs {argv:?}");
    }
    assert!(
        !held.is_empty(),
        "the agent's command is not running just before the restart: no `cat` of {} under the daemon (pid {}).\nIts process tree:\n{}\nThe agent's pane:\n{}",
        hold.path().display(),
        live.pid(),
        process_tree(live.pid()),
        pane_screen(&live, &unit_agent)
    );

    // The restart: SIGTERM to this daemon's own pid, and another on the same
    // state directory.
    // SAFETY: kill(2) on a pid this test spawned and still holds.
    assert_eq!(
        unsafe { libc::kill(live.pid() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(
        live.wait_for_exit(common::load_scaled(Duration::from_secs(30))),
        "the daemon must exit on SIGTERM"
    );
    let next_log = scratch.path().join("deck-next.log");
    let next_log_env = next_log.to_string_lossy().into_owned();
    let mut next_env = env.clone();
    next_env.push(("DOT_AGENT_DECK_LOG", &next_log_env));
    let mut next = inbox::daemon(&next_env);
    assert!(next.is_alive_public(), "the next daemon must start");
    let _gone = std::mem::replace(&mut live, next);
    drop(caller);
    // For the record only: whether a `cat` of this run's FIFO outlived the
    // restart. Dropping `hold` releases it; nothing is signalled.
    let outlived = processes()
        .into_iter()
        .filter(|(_, _, argv)| hold.held_by(argv))
        .count();
    eprintln!("held: after the restart, {outlived} `cat` of this run's FIFO still running");

    // The restart interrupted the unit's work: its attempt ended
    // `interrupted`, and its work reported nothing (a finished unit's kept
    // report would list it too).
    let (unit_work, unit_attempt) = unit_attempt;
    let ended = journal::wait_for(&state, "the unit's attempt's end", |lines| {
        journal::find(lines, "attempt_ended", |line| {
            line["attempt"] == unit_attempt
        })
        .cloned()
    });
    assert_eq!(
        ended["how"],
        "interrupted",
        "the restart ended the unit's attempt as interrupted: {ended}\n{}",
        journal::dump(&state)
    );
    let lines = journal::lines(&state);
    assert!(
        journal::find(&lines, "reported", |line| line["work"] == unit_work).is_none(),
        "the unit reported nothing before the restart:\n{}",
        journal::dump(&state)
    );

    // The person's CLI names the unit, its branch and its worktree.
    let out = person(&live, &repo, &["interrupted", "list", "--json", "--no-git"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let list: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("the list is JSON ({e})\n{}", text(&out)));
    let item = list["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["branch"] == branch.as_str()))
        .unwrap_or_else(|| panic!("`interrupted list` names the unit's branch {branch}: {list}"));
    assert_eq!(item["kind"], "unit", "{item}");
    assert!(
        item["reports"].as_array().is_none_or(|r| r.is_empty()),
        "the unit is listed for its interruption, not for a kept report: {item}"
    );
    let canonical = std::fs::canonicalize(&worktree).expect("the worktree is on disk");
    assert!(
        item["worktree"]
            .as_str()
            .is_some_and(|w| Path::new(w) == worktree.as_path()
                || std::fs::canonicalize(w).is_ok_and(|w| w == canonical)),
        "the item names the unit's worktree {}: {item}",
        worktree.display()
    );
    let human = person(&live, &repo, &["interrupted", "list", "--no-git"]);
    eprintln!("narrative: `interrupted list --no-git`:\n{}", text(&human));
    // The sentinel is ignored and the agent was told to change nothing else:
    // a clean tree, which reclaim removes.
    let dirty = porcelain(&worktree);
    assert!(
        dirty.is_empty(),
        "the agent left the worktree dirty, so reclaim would keep it: {dirty:?}\nThe agent's pane before the restart is gone; daemon log ({}):\n{}",
        first_log.display(),
        log_tail(&first_log, 40)
    );

    // A fresh TUI: the footer, `I`, and reclaim.
    let deck = common::TuiDeck::builder()
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            live.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            live.hook_socket.to_string_lossy().to_string(),
        )
        .without_agent_credentials()
        .launch_with_fixture("minimal");
    // The unit and the dispatcher's plain pane.
    deck.wait_for_string(&interrupted_footer(2));
    deck.send_keys(b"I");
    deck.wait_until_grid("the list names the unit's branch", |g| {
        g.contains(INTERRUPTED_WORK_TITLE) && g.contains(&branch)
    });
    // The unit is listed before "Other agents", so it is the selected row.
    deck.send_keys(b"r");
    deck.wait_until_grid("the dry run's verdict", |g| {
        g.contains(RECLAIM_WOULD_REMOVE)
    });
    assert!(
        deck.snapshot_grid().contains(RECLAIM_EXPLANATION) && worktree.is_dir(),
        "the dry run explains reclaim and removes nothing:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_for_string("y/n");
    deck.send_keys(b"y");
    deck.wait_until_grid("the reclaim's sentence and the footer for the pane", |g| {
        g.contains(RECLAIM_REMOVED) && g.contains(&interrupted_footer(1))
    });
    assert!(
        !worktree.exists(),
        "reclaim removed the worktree {}",
        worktree.display()
    );
    assert!(
        branch_listed(&repo, &branch),
        "reclaim keeps the unit's branch"
    );
    let after = person(&live, &repo, &["interrupted", "list", "--json", "--no-git"]);
    let after: Value = serde_json::from_slice(&after.stdout).expect("the list is JSON");
    assert!(
        !after["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i["branch"] == branch.as_str())),
        "a reclaimed unit is not listed: {after}"
    );
}
