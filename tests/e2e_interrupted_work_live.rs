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
//! directory. The agent's first-run onboarding and the worktree's trust are
//! seeded into the daemon's HOME before the dispatch, as
//! `scheduler/dispatch/013` seeds them, with the deck's Claude Code hooks,
//! so the unattended `claude` takes its first prompt with no keystroke.
//!
//! Cost: one short Haiku turn (one Bash command). Lane 2: it reaches a real
//! agent, so it runs on a developer's machine and never in CI.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/work_journal.rs"]
mod journal;
#[path = "support/inbox_route.rs"]
mod route;

use dot_agent_deck::interrupted_work::{
    INTERRUPTED_WORK_TITLE, RECLAIM_EXPLANATION, RECLAIM_KEPT_DIRTY, RECLAIM_REMOVED,
    RECLAIM_WOULD_REMOVE, interrupted_footer,
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

/// Scenario: A stand-in dispatcher pane dispatches a `--single` unit whose agent is a real interactive Claude Code on Haiku, told to write a uniquely named sentinel file under the worktree's ignored `.dot-agent-deck/` and then wait; once the file is there the daemon is restarted on the same state directory.
/// The person's `interrupted list` then names the unit with its branch and worktree, and a fresh TUI shows the "interrupted items" footer, lists the unit under "Interrupted work" after `I`, and reclaims it with `r` and `y`.
/// The worktree is gone and the branch is kept; if the agent left the tree dirty after all, the TUI's reclaim says it kept the worktree, and that is accepted.
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
    ];
    if let Some(key) = api_key.as_deref() {
        env.push(("ANTHROPIC_API_KEY", key));
    }
    let mut live = inbox::daemon_unbound(&env);

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
    common::seed_claude_worker_home(&live.home, &trust_spellings(&worktree))
        .expect("seed the agent's credentials, onboarding and the worktree's trust");
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

    let caller = route::start(&live, "interrupted-live-caller", json!({"cwd": repo}));
    inbox::status(&live, &caller, "session_start", "codex");
    inbox::status(&live, &caller, "thinking", "codex");
    let task = format!(
        "Use the Bash tool to run exactly this one command now: \
         mkdir -p .dot-agent-deck && echo done > .dot-agent-deck/{SENTINEL} \
         Do not run any other command, do not create or change any other file, do not \
         commit, and do not report back yet. Once the command has run, stop and wait for \
         further instructions."
    );
    // The directive is also the agent's first prompt, on its command line
    // and before `--allowedTools`, which takes every word after it:
    // whether the deck's own delivery of the task reaches a fresh claude's
    // input box is not what this test is about, and in 4 of 6 local runs on
    // 2026-11-20 it had not within 240 s (the claude pane idle and empty).
    let agent_command = format!(
        "claude '{}' --ax-screen-reader --model {HAIKU_MODEL} --allowedTools Bash Read Write",
        task.replace('\'', "'\\''")
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

    // The real agent does its one command.
    let sentinel = worktree.join(".dot-agent-deck").join(SENTINEL);
    let began = std::time::Instant::now();
    const SENTINEL_WAIT: Duration = Duration::from_secs(240);
    assert!(
        common::wait_until(SENTINEL_WAIT, || sentinel.is_file()),
        "the real Haiku agent never wrote {} within {}s.\nThe agent's pane:\n{}\nDaemon stderr:\n{}",
        sentinel.display(),
        SENTINEL_WAIT.as_secs(),
        pane_screen(&live, &unit_agent),
        std::fs::read_to_string(live.home.parent().unwrap().join("daemon-stderr.log"))
            .unwrap_or_default()
    );
    eprintln!(
        "timing: the agent wrote the sentinel {:?} after the dispatch",
        began.elapsed()
    );
    journal::wait_for(&state, "both launches' attempts", |lines| {
        [&caller.agent, &unit_agent]
            .iter()
            .all(|agent| journal::attempt_of(lines, agent).is_some())
            .then_some(())
    });

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
    let mut next = inbox::daemon_unbound(&env);
    assert!(next.is_alive_public(), "the next daemon must start");
    let _gone = std::mem::replace(&mut live, next);
    drop(caller);

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
    let dirty = porcelain(&worktree);
    eprintln!("narrative: the worktree's status after the agent: {dirty:?}");

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
        g.contains(RECLAIM_WOULD_REMOVE) || g.contains(RECLAIM_KEPT_DIRTY)
    });
    if deck.snapshot_grid().contains(RECLAIM_KEPT_DIRTY) {
        // Allowed: the agent changed the tree despite its prompt. The
        // worktree is kept and the branch with it.
        eprintln!("narrative: the agent left the worktree dirty ({dirty:?}); reclaim kept it");
        assert!(worktree.is_dir(), "a kept worktree stays on disk");
        assert!(branch_listed(&repo, &branch), "the branch is kept");
        return;
    }
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
