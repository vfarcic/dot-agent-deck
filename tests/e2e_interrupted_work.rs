#![cfg(all(feature = "e2e", unix))]

//! L2 coverage for the interrupted work list (PRD #1790 stage 2): work a
//! daemon restart left behind, read from the work journal by the real
//! `dot-agent-deck interrupted` CLI against a real `daemon serve`.
//!
//! The restart is the one `tests/e2e_work_journal.rs` uses: a daemon with its
//! own `DOT_AGENT_DECK_STATE_DIR`, launches made through it with token-free
//! routing stand-ins, a signal to that daemon's own pid, and a second daemon
//! on the same state directory. The CLI then runs from outside every pane,
//! as a person at a terminal does, unless a test says otherwise.
//!
//! # The CLI contract these tests hold the binary to
//!
//! ```text
//! dot-agent-deck interrupted list [--json] [--no-git]
//! dot-agent-deck interrupted show <id>
//! dot-agent-deck interrupted reclaim <id> [--dry-run]
//! dot-agent-deck interrupted dismiss <id>...
//! dot-agent-deck interrupted dismiss --all [--yes]
//! ```
//!
//! - `<id>` is an item's id (its root work id), or its unique name, branch or
//!   id suffix.
//! - `list --json` prints the daemon's list as it answered it: `{"items":
//!   [...], "incomplete": bool}`, each item carrying `id`, `kind` (`unit`,
//!   `orchestration`, `issue_dispatch` or `agent`), `name`, `branch`,
//!   `worktree`, `launcher`, `last_entry {kind, ts_ms}` and `reports`
//!   (`report_id`, `kept_until_ms`). `--no-git` leaves out the Git and pull
//!   request facts, so no `gh` runs.
//! - `list` without `--json` prints the title "Interrupted work", the
//!   dispatched and orchestrated items with their ids, then plain agents
//!   under the heading "Other agents" (decision D1), a kept report as
//!   "kept until <date>" (decision D7), and the footer "N interrupted items
//!   from before the restart". Every sentence is one of the wording
//!   constants of `dot_agent_deck::interrupted_work` (decision D2).
//! - `show <id>` prints the item and the body of each kept report, framed as
//!   untrusted text written by an agent.
//! - `reclaim` removes the item's worktree and keeps its branch, and prints
//!   the verdict's sentence; with `--dry-run` it prints what it would do and
//!   changes nothing. A dirty worktree, or one holding other worktrees, is
//!   kept, and the nested worktrees are named.
//! - `dismiss` stops listing the named items, which stay dismissed across
//!   restarts. `dismiss --all` asks first; with no terminal to ask on and no
//!   `--yes`, it refuses and dismisses nothing.
//! - `reclaim` and `dismiss` are refused when the shell carries a deck pane's
//!   identity (an agent, decision D4); `list` and `show` are not.
//! - Exit codes: 0 when the command did what it was asked (`list`, `show`, a
//!   reclaim that removed or would remove, a dismissal of every named item);
//!   1 when it was refused, kept the worktree or named nothing listed; 2
//!   when the daemon does not support interrupted work.
//!
//! `close <unit>` after a restart still answers `unknown-unit`, and now
//! names `dot-agent-deck interrupted list`.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;
// The lane-2 fixture's FIFO hold (`journal/interrupted/028`), here so its
// deterministic tests, which reach no agent, run in lane 1 as well.
#[path = "support/fifo_hold.rs"]
mod fifo_hold;
#[path = "support/work_journal.rs"]
mod journal;
#[path = "support/inbox_route.rs"]
mod route;

use dot_agent_deck::interrupted_work::{
    DISMISSED, INTERRUPTED_WORK_TITLE, KEPT_UNTIL, LABEL_PROGRESS, NOTHING_INTERRUPTED,
    OTHER_AGENTS_GROUP, RECLAIM_EXPLANATION, RECLAIM_KEPT_DIRTY, RECLAIM_KEPT_HOLDS_WORKTREES,
    RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE, REFUSED_AGENT_CALLER, REFUSED_NOT_OWNED,
    REPORT_NOT_OF_ITEM, interrupted_footer,
};
use inbox::Pane;
use serde_json::{Value, json};
use spec::spec;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

/// The orchestration an orchestration-shaped unit is dispatched as.
const TEAM: &str = "interrupted-team";

/// A sequence of daemons on one state directory: the one serving now, and
/// the ones a restart ended, kept so their homes outlive the test.
struct Decks {
    live: common::DaemonProc,
    gone: Vec<common::DaemonProc>,
    state_env: String,
    state: PathBuf,
    /// The fixture repository the units are dispatched from.
    repo: PathBuf,
    _scratch: tempfile::TempDir,
}

impl Decks {
    /// A daemon on a fresh state directory, and a one-commit repository that
    /// defines a one-role orchestration and ignores what the stand-ins and
    /// the tests write into a worktree.
    fn start() -> Self {
        let scratch = common::harness_tempdir().unwrap();
        let state = scratch.path().join("state");
        let state_env = state.to_string_lossy().into_owned();
        let live = inbox::daemon_unbound(&[("DOT_AGENT_DECK_STATE_DIR", &state_env)]);
        let repo = scratch.path().join("interrupted-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let cmd = route::command(&live.home);
        std::fs::write(
            repo.join(".dot-agent-deck.toml"),
            format!(
                "[[orchestrations]]\nname = '{TEAM}'\n[[orchestrations.roles]]\nname = 'orchestrator'\nstart = true\ncommand = {}\n",
                serde_json::to_string(&cmd).unwrap()
            ),
        )
        .unwrap();
        journal::init_repo(&repo);
        // The stand-ins write `.route-<agent>.*` beside themselves, the
        // harness its `.test-capability-<agent>`, and the deck its
        // `.dot-agent-deck/`: ignored, so a unit's worktree reads
        // clean until a test makes it dirty. Linked worktrees share this file.
        std::fs::write(
            repo.join(".git").join("info").join("exclude"),
            "/.dot-agent-deck/\n.route-*\n.test-capability-*\n/.worktrees/\n",
        )
        .unwrap();
        live.approve_project_trust(&repo);
        Decks {
            live,
            gone: Vec::new(),
            state_env,
            state,
            repo,
            _scratch: scratch,
        }
    }

    /// End the serving daemon with SIGTERM to its own pid, as a deck restart
    /// does, and start another on the same state directory.
    fn restart(&mut self) {
        // The daemon's own pid, never a pattern (CLAUDE.md rule 12).
        // SAFETY: kill(2) on a pid this test spawned and still holds.
        assert_eq!(
            unsafe { libc::kill(self.live.pid() as libc::pid_t, libc::SIGTERM) },
            0
        );
        assert!(
            self.live
                .wait_for_exit(common::load_scaled(Duration::from_secs(20))),
            "the daemon must exit on SIGTERM"
        );
        let mut next = inbox::daemon_unbound(&[("DOT_AGENT_DECK_STATE_DIR", &self.state_env)]);
        assert!(next.is_alive_public(), "the next daemon must start");
        self.gone.push(std::mem::replace(&mut self.live, next));
    }

    /// The routing stand-in pane `name`, started in the repository.
    fn pane(&self, name: &str) -> Pane {
        route::start(&self.live, name, json!({"cwd": self.repo}))
    }

    /// The first agent of a unit named `unit` that `caller` dispatched with
    /// `shape` (`--single` or `--orchestration=<name>`), once it runs.
    fn dispatch(&self, caller: &Pane, unit: &str, shape: &str) -> Pane {
        inbox::status(&self.live, caller, "session_start", "codex");
        inbox::status(&self.live, caller, "thinking", "codex");
        let before = route::messages(&self.live, caller).len();
        route::cli(
            &self.live,
            caller,
            &self.repo,
            &["dispatch", unit, "--task", "INTERRUPTED-WORK-TASK", shape],
        );
        self.dispatch_answered(caller, unit, before);
        let suffix = format!("-dispatch-{unit}");
        let found = std::cell::RefCell::new(None);
        let budget = common::load_scaled(Duration::from_secs(20));
        assert!(
            common::wait_until(budget, || {
                *found.borrow_mut() = common::agent_records_on(&self.live.attach_socket)
                    .into_iter()
                    .find(|r| {
                        r.id != caller.agent
                            && r.cwd.as_deref().is_some_and(|c| c.ends_with(&suffix))
                    })
                    .map(|r| r.id);
                found.borrow().is_some()
            }),
            "the dispatch of {unit} never spawned within {budget:?}: {:?}",
            common::agent_records_on(&self.live.attach_socket)
        );
        route::observe(&self.live, &found.into_inner().unwrap())
    }

    /// Wait until the dispatch of `unit` that `caller` just ran has answered
    /// it: a message beyond the `before` it already held. The `dispatch` CLI
    /// returns once the daemon accepts the request, and the daemon answers
    /// only when the spawn is done, so a restart before this could stop the
    /// dispatch itself, whose rollback then removes the unit's new worktree.
    fn dispatch_answered(&self, caller: &Pane, unit: &str, before: usize) {
        let budget = common::load_scaled(Duration::from_secs(20));
        assert!(
            common::wait_until(budget, || route::messages(&self.live, caller).len()
                > before),
            "the dispatch of {unit} never answered its caller within {budget:?}: {:?}",
            route::messages(&self.live, caller)
        );
    }

    /// `agent`'s working directory, as the serving daemon recorded it.
    fn cwd_of(&self, agent: &str) -> PathBuf {
        common::agent_records_on(&self.live.attach_socket)
            .into_iter()
            .find(|r| r.id == agent)
            .and_then(|r| r.cwd)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("agent {agent} has no recorded cwd"))
    }

    /// Wait until the journal holds an `attempt_started` for every one of
    /// `panes`, so a restart cannot outrun the lines it is about.
    fn journaled(&self, panes: &[&Pane]) {
        journal::wait_for(&self.state, "every launch's attempt", |lines| {
            panes
                .iter()
                .all(|p| journal::attempt_of(lines, &p.agent).is_some())
                .then_some(())
        });
    }

    /// The real CLI as a person at a terminal runs it: no pane identity, no
    /// terminal on stdin.
    fn person(&self, args: &[&str]) -> Output {
        std::process::Command::new(common::contained_deck_binary())
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.live.home)
            .env("DOT_AGENT_DECK_SOCKET", &self.live.hook_socket)
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &self.live.attach_socket)
            .stdin(std::process::Stdio::null())
            .current_dir(&self.repo)
            .output()
            .expect("run the deck CLI")
    }

    /// The real CLI as `pane`'s agent runs it: the pane's identity in its
    /// environment.
    fn agent(&self, pane: &Pane, args: &[&str]) -> Output {
        route::cli_command(&self.live, pane, &self.repo, args)
            .env("DOT_AGENT_DECK_ATTACH_SOCKET", &self.live.attach_socket)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the deck CLI as an agent")
    }

    /// A fresh TUI, the real binary in a PTY, attached to the serving daemon.
    fn tui(&self) -> common::TuiDeck {
        common::TuiDeck::builder()
            .with_env(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                self.live.attach_socket.to_string_lossy().to_string(),
            )
            .with_env(
                "DOT_AGENT_DECK_SOCKET",
                self.live.hook_socket.to_string_lossy().to_string(),
            )
            .launch_with_fixture("minimal")
    }

    /// `interrupted list --json --no-git`, which must succeed.
    fn list(&self) -> Value {
        let out = self.person(&["interrupted", "list", "--json", "--no-git"]);
        assert_exit("interrupted list --json --no-git", &out, 0);
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "`interrupted list --json` must print the list as JSON ({e})\n{}",
                text(&out)
            )
        })
    }
}

fn text(out: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assert_exit(label: &str, out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{label}: expected exit {code}\n{}",
        text(out)
    );
}

fn items(list: &Value) -> &[Value] {
    list["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("the list has an `items` array: {list}"))
}

/// The listed item of the unit named `unit`, found by its branch.
fn unit_item<'a>(list: &'a Value, unit: &str) -> Option<&'a Value> {
    let branch = format!("agent/dispatch-{unit}");
    items(list).iter().find(|i| i["branch"] == branch.as_str())
}

fn id_of(item: &Value) -> String {
    item["id"]
        .as_str()
        .unwrap_or_else(|| panic!("an item has a string id: {item}"))
        .to_string()
}

fn listed(list: &Value, id: &str) -> bool {
    items(list).iter().any(|i| i["id"] == id)
}

/// Whether the item names `worktree` as its worktree, in either spelling.
fn names_worktree(item: &Value, worktree: &Path) -> bool {
    let canonical = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    item["worktree"].as_str().is_some_and(|w| {
        Path::new(w) == worktree
            || std::fs::canonicalize(w).is_ok_and(|w| w == canonical)
            || Path::new(w) == canonical
    })
}

fn porcelain(dir: &Path) -> String {
    let out = common::fixture_git(dir, dir)
        .args(["status", "--porcelain"])
        .output()
        .expect("git available");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn branch_listed(repo: &Path, branch: &str) -> bool {
    let out = common::fixture_git(repo, repo)
        .args(["branch", "--list", branch])
        .output()
        .expect("git available");
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

/// The `reported` line the journal holds for a report from `pane`'s work.
fn reported_by(decks: &Decks, pane: &Pane) -> Value {
    journal::wait_for(&decks.state, "the unit's `reported` line", |lines| {
        let work = journal::attempt_of(lines, &pane.agent)?["work"].clone();
        journal::find(lines, "reported", |l| l["work"] == work).cloned()
    })
}

/// Scenario: Through one daemon, a stand-in pane dispatches a `--single`
/// unit and an orchestration-shaped unit. While they run, `interrupted list`
/// shows none of them. The daemon is stopped and another started on the
/// same state directory; `interrupted list --json --no-git` then shows the
/// two units with their kind, branch, worktree, launcher and last journal
/// entry and how far each got (its attempts), and the pane as a plain agent,
/// and the human list shows the title, the units, a progress line for each
/// item, the pane under "Other agents" and the footer.
#[spec("journal/interrupted/001")]
#[test]
fn interrupted_001_a_restart_lists_both_units_and_the_plain_pane() {
    const SINGLE: &str = "interrupted-single";
    const TEAM_UNIT: &str = "interrupted-team-unit";
    const CALLER: &str = "interrupted-caller";
    let mut decks = Decks::start();
    let caller = decks.pane(CALLER);
    let single = decks.dispatch(&caller, SINGLE, "--single");
    let team = decks.dispatch(&caller, TEAM_UNIT, &format!("--orchestration={TEAM}"));
    let single_tree = decks.cwd_of(&single.agent);
    let team_tree = decks.cwd_of(&team.agent);
    decks.journaled(&[&caller, &single, &team]);

    let before = decks.list();
    assert!(
        items(&before).is_empty(),
        "running work is not interrupted work: {before}"
    );

    decks.restart();
    drop((caller, single, team));
    let list = decks.list();
    assert_eq!(list["incomplete"], false, "{list}");
    assert_eq!(
        items(&list).len(),
        3,
        "two units and one plain agent: {list}"
    );
    let ts_ms = |item: &Value| item["last_entry"]["ts_ms"].as_u64().unwrap_or(0);
    for (unit, tree) in [(SINGLE, &single_tree), (TEAM_UNIT, &team_tree)] {
        let item = unit_item(&list, unit)
            .unwrap_or_else(|| panic!("the unit {unit} is listed after the restart: {list}"));
        assert_eq!(item["kind"], "unit", "{item}");
        assert!(
            names_worktree(item, tree),
            "the unit's worktree is {}: {item}",
            tree.display()
        );
        assert_eq!(
            item["launcher"], CALLER,
            "the dispatcher pane launched it: {item}"
        );
        assert!(
            item["last_entry"]["kind"]
                .as_str()
                .is_some_and(|k| !k.is_empty())
                && ts_ms(item) > 0,
            "the item names its last journal line: {item}"
        );
        assert!(
            item["progress"]["attempts"].as_u64().unwrap_or(0) >= 1,
            "the item counts the attempts that ran under it: {item}"
        );
    }
    let agent = items(&list)
        .iter()
        .find(|i| i["kind"] == "agent")
        .unwrap_or_else(|| panic!("the plain pane is listed as an agent: {list}"));
    assert_eq!(agent["name"], CALLER, "{agent}");
    assert!(agent["branch"].is_null(), "{agent}");

    let human = decks.person(&["interrupted", "list", "--no-git"]);
    assert_exit("interrupted list --no-git", &human, 0);
    let out = String::from_utf8_lossy(&human.stdout).into_owned();
    let at = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("the list prints {needle:?}:\n{out}"))
    };
    let title = at(INTERRUPTED_WORK_TITLE);
    let group = at(OTHER_AGENTS_GROUP);
    for unit in [SINGLE, TEAM_UNIT] {
        let id = id_of(unit_item(&list, unit).unwrap());
        let unit_at = at(&id);
        assert!(
            title < unit_at && unit_at < group,
            "the unit {unit} is listed under the title, before {OTHER_AGENTS_GROUP:?}:\n{out}"
        );
    }
    assert!(
        at(&id_of(agent)) > group,
        "the plain pane is listed under {OTHER_AGENTS_GROUP:?}:\n{out}"
    );
    assert_eq!(
        out.matches(&format!("\n    {LABEL_PROGRESS}: ")).count(),
        3,
        "every item prints how far it got:\n{out}"
    );
    at(&interrupted_footer(3));
}

/// Scenario: A dispatched unit and its dispatcher pane are interrupted by a
/// restart. `interrupted dismiss <unit id>` says it was dismissed, the unit
/// leaves the list and the pane stays, and after a second restart the unit
/// is still gone. `dismiss --all` with no terminal and no `--yes` refuses
/// and dismisses nothing; `dismiss --all --yes` empties the list.
#[spec("journal/interrupted/002")]
#[test]
fn interrupted_002_a_dismissal_survives_a_second_restart() {
    const UNIT: &str = "interrupted-dismiss";
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-dismisser");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    decks.journaled(&[&caller, &unit]);
    decks.restart();
    drop((caller, unit));

    let list = decks.list();
    let id = id_of(unit_item(&list, UNIT).unwrap_or_else(|| panic!("listed: {list}")));
    assert_eq!(items(&list).len(), 2, "the unit and its pane: {list}");

    let out = decks.person(&["interrupted", "dismiss", &id]);
    assert_exit("interrupted dismiss <id>", &out, 0);
    assert!(text(&out).contains(DISMISSED), "{}", text(&out));
    let after = decks.list();
    assert!(
        !listed(&after, &id),
        "a dismissed item is not listed: {after}"
    );
    assert_eq!(
        items(&after).len(),
        1,
        "only the unit was dismissed: {after}"
    );

    decks.restart();
    let again = decks.list();
    assert!(
        !listed(&again, &id),
        "a dismissal survives a restart: {again}"
    );
    assert_eq!(items(&again).len(), 1, "the pane is still listed: {again}");

    let unconfirmed = decks.person(&["interrupted", "dismiss", "--all"]);
    assert_exit(
        "interrupted dismiss --all, no terminal, no --yes",
        &unconfirmed,
        1,
    );
    assert_eq!(
        items(&decks.list()).len(),
        1,
        "an unconfirmed dismiss --all dismisses nothing"
    );
    let all = decks.person(&["interrupted", "dismiss", "--all", "--yes"]);
    assert_exit("interrupted dismiss --all --yes", &all, 0);
    let empty = decks.list();
    assert!(items(&empty).is_empty(), "{empty}");
}

/// Scenario: A dispatched unit's clean, deck-created worktree is left by a
/// restart. `interrupted reclaim <id> --dry-run` says it would remove the
/// worktree and keep the branch, and changes nothing; `interrupted reclaim
/// <id>` says it did, the worktree directory is gone, `git branch --list`
/// still shows the unit's branch, and the item leaves the list.
#[spec("journal/interrupted/003")]
#[test]
fn interrupted_003_reclaim_removes_a_clean_worktree_and_keeps_its_branch() {
    const UNIT: &str = "interrupted-reclaim";
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-reclaimer");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    let worktree = decks.cwd_of(&unit.agent);
    decks.journaled(&[&caller, &unit]);
    decks.restart();
    drop((caller, unit));
    assert!(
        dot_agent_deck::worktree_owner::is_marked(&worktree),
        "premise: the deck marked the worktree it created"
    );
    assert_eq!(porcelain(&worktree), "", "premise: the worktree is clean");

    let list = decks.list();
    let id = id_of(unit_item(&list, UNIT).unwrap_or_else(|| panic!("listed: {list}")));
    assert_eq!(
        marked_launch(&worktree),
        Some(id.clone()),
        "premise: the marker names the launch the item is rooted at"
    );

    let dry = decks.person(&["interrupted", "reclaim", &id, "--dry-run"]);
    assert_exit("interrupted reclaim --dry-run", &dry, 0);
    assert!(text(&dry).contains(RECLAIM_WOULD_REMOVE), "{}", text(&dry));
    assert!(worktree.is_dir(), "a dry run removes nothing");
    assert!(
        listed(&decks.list(), &id),
        "a dry run leaves the item listed"
    );

    let out = decks.person(&["interrupted", "reclaim", &id]);
    assert_exit("interrupted reclaim", &out, 0);
    assert!(text(&out).contains(RECLAIM_REMOVED), "{}", text(&out));
    assert!(
        !worktree.exists(),
        "reclaim removed the worktree {}",
        worktree.display()
    );
    assert!(
        branch_listed(&decks.repo, &format!("agent/dispatch-{UNIT}")),
        "reclaim keeps the unit's branch"
    );
    let after = decks.list();
    assert!(
        !listed(&after, &id),
        "a reclaimed item is not listed: {after}"
    );
}

/// The launch the ownership marker of `worktree` names, as the work id's
/// text; `None` when it names none or there is no marker.
fn marked_launch(worktree: &Path) -> Option<String> {
    use dot_agent_deck::worktree_owner::{MarkedLaunch, git_dir_of, marked_launch_in};
    match marked_launch_in(&git_dir_of(worktree)?) {
        MarkedLaunch::Names(work) => Some(work.as_str().to_string()),
        MarkedLaunch::NotMarked | MarkedLaunch::Unnamed => None,
    }
}

/// Scenario: Two dispatched units are left by a restart. One worktree gets
/// an uncommitted file; the other gets a nested worktree under an ignored
/// directory, so it reads clean. `interrupted reclaim` on the first keeps
/// it, says it has uncommitted changes and leaves it listed; on the second
/// it keeps it, says other worktrees lie inside it and names the nested one.
#[spec("journal/interrupted/004")]
#[test]
fn interrupted_004_reclaim_keeps_a_dirty_worktree_and_one_holding_another() {
    const DIRTY: &str = "interrupted-dirty";
    const HOLDER: &str = "interrupted-holder";
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-keeper");
    let dirty = decks.dispatch(&caller, DIRTY, "--single");
    let holder = decks.dispatch(&caller, HOLDER, "--single");
    let dirty_tree = decks.cwd_of(&dirty.agent);
    let holder_tree = decks.cwd_of(&holder.agent);
    decks.journaled(&[&caller, &dirty, &holder]);
    decks.restart();
    drop((caller, dirty, holder));

    let wip = dirty_tree.join("uncommitted-1790.txt");
    std::fs::write(&wip, "uncommitted work").unwrap();
    assert_ne!(porcelain(&dirty_tree), "", "premise: the worktree is dirty");
    let nested = holder_tree.join(".worktrees").join("nested-1790");
    let added = common::fixture_git(&holder_tree, &holder_tree)
        .args(["worktree", "add", "-q", "-b", "feat/nested-1790"])
        .arg(&nested)
        .output()
        .expect("git available");
    assert!(
        added.status.success(),
        "nested `git worktree add`: {added:?}"
    );
    let nested_wip = nested.join("nested-wip-1790.txt");
    std::fs::write(&nested_wip, "work in a nested worktree").unwrap();
    assert_eq!(
        porcelain(&holder_tree),
        "",
        "premise: the holder reads clean, its nested worktree ignored"
    );

    let list = decks.list();
    let dirty_id = id_of(unit_item(&list, DIRTY).unwrap_or_else(|| panic!("{list}")));
    let holder_id = id_of(unit_item(&list, HOLDER).unwrap_or_else(|| panic!("{list}")));

    let kept = decks.person(&["interrupted", "reclaim", &dirty_id]);
    assert_exit("reclaim of a dirty worktree", &kept, 1);
    assert!(text(&kept).contains(RECLAIM_KEPT_DIRTY), "{}", text(&kept));
    assert_eq!(
        std::fs::read_to_string(&wip).unwrap_or_default(),
        "uncommitted work",
        "the uncommitted file is kept"
    );
    assert!(listed(&decks.list(), &dirty_id), "a kept item stays listed");

    let held = decks.person(&["interrupted", "reclaim", &holder_id]);
    assert_exit("reclaim of a worktree holding another", &held, 1);
    let said = text(&held);
    assert!(said.contains(RECLAIM_KEPT_HOLDS_WORKTREES), "{said}");
    let canonical = std::fs::canonicalize(&nested).unwrap();
    assert!(
        said.contains(&*nested.to_string_lossy()) || said.contains(&*canonical.to_string_lossy()),
        "the nested worktree {} is named:\n{said}",
        nested.display()
    );
    assert!(holder_tree.is_dir(), "the holder is kept");
    assert_eq!(
        std::fs::read_to_string(&nested_wip).unwrap_or_default(),
        "work in a nested worktree",
        "the nested worktree's work is kept"
    );
    assert!(
        listed(&decks.list(), &holder_id),
        "a kept item stays listed"
    );
}

/// Scenario: A stand-in pane dispatches a `--single` unit with `--dir` into
/// a clean linked worktree the user made with `git worktree add`, and the
/// daemon restarts. The unit is listed with that directory as its worktree;
/// `interrupted reclaim` on it refuses because the deck did not create the
/// worktree, and the directory, its file and its branch are all kept.
#[spec("journal/interrupted/036")]
#[test]
fn interrupted_036_reclaim_keeps_a_directory_a_dir_dispatch_started_in() {
    const UNIT: &str = "interrupted-dir";
    let mut decks = Decks::start();
    let target = decks.repo.parent().unwrap().join("users-own-worktree-1790");
    let added = common::fixture_git(&decks.repo, &decks.repo)
        .args(["worktree", "add", "-q", "-b", "feat/users-own-1790"])
        .arg(&target)
        .output()
        .expect("git available");
    assert!(added.status.success(), "`git worktree add`: {added:?}");
    let target = std::fs::canonicalize(&target).unwrap();
    let kept_file = target.join("README.md");
    assert!(kept_file.is_file(), "premise: the worktree has the fixture");

    let caller = decks.pane("interrupted-dir-caller");
    inbox::status(&decks.live, &caller, "session_start", "codex");
    inbox::status(&decks.live, &caller, "thinking", "codex");
    let before = route::messages(&decks.live, &caller).len();
    route::cli(
        &decks.live,
        &caller,
        &decks.repo,
        &[
            "dispatch",
            UNIT,
            "--dir",
            &target.to_string_lossy(),
            "--task",
            "INTERRUPTED-WORK-TASK",
            "--single",
        ],
    );
    let found = std::cell::RefCell::new(None);
    let budget = common::load_scaled(Duration::from_secs(20));
    assert!(
        common::wait_until(budget, || {
            *found.borrow_mut() = common::agent_records_on(&decks.live.attach_socket)
                .into_iter()
                .find(|r| {
                    r.id != caller.agent
                        && r.cwd
                            .as_deref()
                            .is_some_and(|c| std::fs::canonicalize(c).is_ok_and(|c| c == target))
                })
                .map(|r| r.id);
            found.borrow().is_some()
        }),
        "the --dir dispatch never spawned within {budget:?}: {:?}",
        common::agent_records_on(&decks.live.attach_socket)
    );
    let unit = route::observe(&decks.live, &found.into_inner().unwrap());
    decks.dispatch_answered(&caller, UNIT, before);
    decks.journaled(&[&caller, &unit]);
    decks.restart();
    drop((caller, unit));
    assert!(
        !dot_agent_deck::worktree_owner::is_marked(&target),
        "premise: a --dir dispatch marks nothing as the deck's"
    );
    assert_eq!(porcelain(&target), "", "premise: the worktree is clean");

    let list = decks.list();
    let item = items(&list)
        .iter()
        .find(|i| i["branch"] == "feat/users-own-1790")
        .unwrap_or_else(|| panic!("the --dir unit is listed by its branch: {list}"));
    assert!(
        names_worktree(item, &target),
        "the item names the --dir directory: {item}"
    );
    let id = id_of(item);

    let out = decks.person(&["interrupted", "reclaim", &id]);
    assert_exit("reclaim of a --dir unit's directory", &out, 1);
    assert!(text(&out).contains(REFUSED_NOT_OWNED), "{}", text(&out));
    assert!(
        kept_file.is_file(),
        "the directory a --dir dispatch started in is kept"
    );
    assert!(
        branch_listed(&decks.repo, "feat/users-own-1790"),
        "its branch is kept"
    );
    assert!(listed(&decks.list(), &id), "a refused item stays listed");
}

/// Scenario: A dispatched unit sends its report to its dispatcher, which
/// never fetches it, and the daemon restarts. The unit's item lists that
/// report with the date it is kept until, `interrupted show <id>` prints
/// the report's text inside the untrusted-report frame, `interrupted show <id>
/// --report <report id>` prints that one report the same way, and a report id
/// that is not the item's is refused.
#[spec("journal/interrupted/005")]
#[test]
fn interrupted_005_an_unread_report_is_listed_and_shown_after_a_restart() {
    const UNIT: &str = "interrupted-reporter";
    const REPORT: &str = "INTERRUPTED-REPORT-1790: the unit's finding";
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-absent-reader");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    let worktree = decks.cwd_of(&unit.agent);
    decks.journaled(&[&caller, &unit]);
    route::cli(
        &decks.live,
        &unit,
        &worktree,
        &["work-done", "--done", "--task", REPORT],
    );
    let reported = reported_by(&decks, &unit);
    assert_eq!(reported["destination"], "inbox", "{reported}");
    decks.restart();
    drop((caller, unit));

    let budget = common::load_scaled(Duration::from_secs(20));
    let list = std::cell::RefCell::new(Value::Null);
    let has_report = common::wait_until(budget, || {
        *list.borrow_mut() = decks.list();
        unit_item(&list.borrow(), UNIT)
            .is_some_and(|i| i["reports"].as_array().is_some_and(|r| !r.is_empty()))
    });
    let list = list.into_inner();
    assert!(has_report, "the unit's item carries its report: {list}");
    let item = unit_item(&list, UNIT).unwrap();
    let report = &item["reports"][0];
    assert_eq!(report["report_id"], reported["report_id"], "{item}");
    assert!(report["kept_until_ms"].as_u64().is_some(), "{item}");

    let human = decks.person(&["interrupted", "list", "--no-git"]);
    assert_exit("interrupted list --no-git", &human, 0);
    assert!(text(&human).contains(KEPT_UNTIL), "{}", text(&human));

    let shown = decks.person(&["interrupted", "show", &id_of(item)]);
    assert_exit("interrupted show <id>", &shown, 0);
    let out = String::from_utf8_lossy(&shown.stdout).into_owned();
    let open = out
        .find("[UNTRUSTED-WORKER-REPORT:")
        .unwrap_or_else(|| panic!("the report is framed as untrusted:\n{out}"));
    let body = out
        .find(REPORT)
        .unwrap_or_else(|| panic!("the report's text is shown:\n{out}"));
    let close = out
        .find(":END-UNTRUSTED-WORKER-REPORT]")
        .unwrap_or_else(|| panic!("the frame is closed:\n{out}"));
    assert!(open < body && body < close, "inside the frame:\n{out}");

    let report_id = report["report_id"].as_str().unwrap().to_string();
    let one = decks.person(&["interrupted", "show", &id_of(item), "--report", &report_id]);
    assert_exit("interrupted show <id> --report <report id>", &one, 0);
    let out = String::from_utf8_lossy(&one.stdout).into_owned();
    let open = out
        .find("[UNTRUSTED-WORKER-REPORT:")
        .unwrap_or_else(|| panic!("the report is framed as untrusted:\n{out}"));
    let body = out
        .find(REPORT)
        .unwrap_or_else(|| panic!("the report's text is shown:\n{out}"));
    assert!(open < body, "inside the frame:\n{out}");
    let stranger = decks.person(&[
        "interrupted",
        "show",
        &id_of(item),
        "--report",
        "not-this-items-report",
    ]);
    assert_exit("interrupted show <id> --report <another id>", &stranger, 1);
    assert!(
        text(&stranger).contains(REPORT_NOT_OF_ITEM),
        "{}",
        text(&stranger)
    );
}

/// Scenario: With no restart, a dispatched unit's dispatcher pane is closed
/// and the unit then sends its report. `interrupted list` shows the unit's
/// work as an item carrying that report, kept for a caller that is gone.
#[spec("journal/interrupted/006")]
#[test]
fn interrupted_006_a_report_for_a_gone_caller_is_listed_without_a_restart() {
    const UNIT: &str = "interrupted-orphan";
    let decks = Decks::start();
    let caller = decks.pane("interrupted-gone-caller");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    let worktree = decks.cwd_of(&unit.agent);
    decks.journaled(&[&caller, &unit]);
    assert!(
        items(&decks.list()).is_empty(),
        "nothing is interrupted while both run"
    );

    let stopped = common::attach_request_on(
        &decks.live.attach_socket,
        &dot_agent_deck::daemon_protocol::AttachRequest::StopAgent {
            id: caller.agent.clone(),
            keep_worktree: false,
        },
    )
    .expect("stop the dispatcher pane");
    assert!(
        stopped.ok,
        "the dispatcher pane closes: {:?}",
        stopped.error
    );
    let _ = route::cli_output(
        &decks.live,
        &unit,
        &worktree,
        &[
            "work-done",
            "--done",
            "--task",
            "INTERRUPTED-ORPHAN-REPORT-1790",
        ],
    );
    let reported = reported_by(&decks, &unit);

    let budget = common::load_scaled(Duration::from_secs(20));
    let list = std::cell::RefCell::new(Value::Null);
    let has_report = common::wait_until(budget, || {
        *list.borrow_mut() = decks.list();
        unit_item(&list.borrow(), UNIT)
            .is_some_and(|i| i["reports"].as_array().is_some_and(|r| !r.is_empty()))
    });
    let list = list.into_inner();
    assert!(
        has_report,
        "the unit's item carries the report its gone caller never read: {list}"
    );
    let item = unit_item(&list, UNIT).unwrap();
    assert_eq!(
        item["reports"][0]["report_id"], reported["report_id"],
        "{item}"
    );
    drop(unit);
}

/// Scenario: After a restart leaves a dispatched unit, an agent pane of the
/// new daemon runs the CLI with its pane identity. `interrupted list` works
/// and shows the unit; `interrupted reclaim` and `interrupted dismiss` are
/// refused as an agent's, and the worktree and the item are left as they
/// were.
#[spec("journal/interrupted/007")]
#[test]
fn interrupted_007_an_agent_may_list_but_not_reclaim_or_dismiss() {
    const UNIT: &str = "interrupted-agent-caller";
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-old-caller");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    let worktree = decks.cwd_of(&unit.agent);
    decks.journaled(&[&caller, &unit]);
    decks.restart();
    drop((caller, unit));
    let agent = decks.pane("interrupted-in-pane-agent");

    let listing = decks.agent(&agent, &["interrupted", "list", "--json", "--no-git"]);
    assert_exit("interrupted list as an agent", &listing, 0);
    let list: Value = serde_json::from_slice(&listing.stdout)
        .unwrap_or_else(|e| panic!("JSON ({e})\n{}", text(&listing)));
    let id = id_of(unit_item(&list, UNIT).unwrap_or_else(|| panic!("{list}")));

    let reclaim = decks.agent(&agent, &["interrupted", "reclaim", &id]);
    assert_exit("interrupted reclaim as an agent", &reclaim, 1);
    assert!(
        text(&reclaim).contains(REFUSED_AGENT_CALLER),
        "{}",
        text(&reclaim)
    );
    let dismiss = decks.agent(&agent, &["interrupted", "dismiss", &id]);
    assert_exit("interrupted dismiss as an agent", &dismiss, 1);
    assert!(
        text(&dismiss).contains(REFUSED_AGENT_CALLER),
        "{}",
        text(&dismiss)
    );
    assert!(worktree.is_dir(), "a refused reclaim removes nothing");
    assert!(
        listed(&decks.list(), &id),
        "a refused dismissal leaves the item listed"
    );
}

/// Scenario: A dispatched unit is left by a restart, and a person runs
/// `close <unit>` against the new daemon. It is refused as `unknown-unit`,
/// and the message points at `dot-agent-deck interrupted list`, in the
/// human output and in `--json`.
#[spec("dispatch/close-verb/015")]
#[test]
fn close_verb_015_after_a_restart_close_points_at_the_interrupted_list() {
    const UNIT: &str = "interrupted-close";
    const HINT: &str = "dot-agent-deck interrupted list";
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-closer");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    decks.journaled(&[&caller, &unit]);
    decks.restart();
    drop((caller, unit));

    let human = decks.person(&["close", UNIT]);
    assert_exit("close <unit> after a restart", &human, 1);
    assert!(text(&human).contains(HINT), "{}", text(&human));

    let json = decks.person(&["close", UNIT, "--json"]);
    assert_exit("close <unit> --json after a restart", &json, 1);
    let report: Value = serde_json::from_slice(&json.stdout)
        .unwrap_or_else(|e| panic!("close --json prints JSON ({e})\n{}", text(&json)));
    let raw = report.to_string();
    assert!(raw.contains("unknown-unit"), "{raw}");
    assert!(raw.contains(HINT), "{raw}");
}

/// Scenario: A dispatched unit and its dispatcher pane are interrupted by a
/// restart, and a fresh TUI attaches to the new daemon. Its footer says "2
/// interrupted items from before the restart"; `I` opens the "Interrupted
/// work" list naming the unit's branch; `d` dismisses the unit (the footer
/// then counts one) and `d` again the pane, which empties the list and
/// removes the footer. After another restart a fresh TUI shows no footer,
/// and its list, opened with `I`, says there is no interrupted work.
#[spec("journal/interrupted/014")]
#[test]
fn interrupted_014_a_fresh_tui_lists_and_dismisses_interrupted_work() {
    const UNIT: &str = "interrupted-tui-dismiss";
    let branch = format!("agent/dispatch-{UNIT}");
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-tui-caller");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    decks.journaled(&[&caller, &unit]);
    decks.restart();
    drop((caller, unit));

    let deck = decks.tui();
    deck.wait_for_string(&interrupted_footer(2));
    deck.send_keys(b"I");
    deck.wait_until_grid("the list names the unit's branch", |g| {
        g.contains(INTERRUPTED_WORK_TITLE) && g.contains(&branch)
    });
    deck.send_keys(b"d");
    deck.wait_until_grid("the dismissed unit leaves the list", |g| {
        g.contains(DISMISSED) && !g.contains(&branch) && g.contains(&interrupted_footer(1))
    });
    deck.send_keys(b"d");
    deck.wait_until_grid("the emptied list removes the footer", |g| {
        g.contains(NOTHING_INTERRUPTED) && !g.contains("interrupted item")
    });
    let after = decks.list();
    assert!(items(&after).is_empty(), "the TUI dismissed both: {after}");
    drop(deck);

    decks.restart();
    let fresh = decks.tui();
    fresh.wait_for_string("No active agents");
    fresh.send_keys(b"I");
    fresh.wait_until_grid("the daemon answered an empty list", |g| {
        g.contains(INTERRUPTED_WORK_TITLE) && g.contains(NOTHING_INTERRUPTED)
    });
    fresh.send_keys(b"\x1b");
    fresh.wait_until_grid("the list closed", |g| !g.contains(NOTHING_INTERRUPTED));
    let grid = fresh.snapshot_grid();
    assert!(
        !grid.contains("interrupted item"),
        "dismissed work stays dismissed after a restart, so no footer:\n{grid}"
    );
}

/// Scenario: A dispatched unit's clean, deck-created worktree is left by a
/// restart, and a fresh TUI attaches. In the "Interrupted work" list, `r` on
/// the unit shows the dry run's verdict ("Reclaim would remove its worktree
/// and keep its branch.") and what reclaim means, and asks y/n, with the
/// worktree still on disk; `y` says it was reclaimed, the worktree directory
/// is gone, the unit's branch is kept and the footer counts only the pane.
#[spec("journal/interrupted/015")]
#[test]
fn interrupted_015_reclaim_from_the_tui_removes_the_worktree_and_keeps_the_branch() {
    const UNIT: &str = "interrupted-tui-reclaim";
    let branch = format!("agent/dispatch-{UNIT}");
    let mut decks = Decks::start();
    let caller = decks.pane("interrupted-tui-reclaimer");
    let unit = decks.dispatch(&caller, UNIT, "--single");
    let worktree = decks.cwd_of(&unit.agent);
    decks.journaled(&[&caller, &unit]);
    decks.restart();
    drop((caller, unit));
    assert!(
        dot_agent_deck::worktree_owner::is_marked(&worktree),
        "premise: the deck marked the worktree it created"
    );
    assert_eq!(porcelain(&worktree), "", "premise: the worktree is clean");

    let deck = decks.tui();
    deck.wait_for_string(&interrupted_footer(2));
    deck.send_keys(b"I");
    deck.wait_until_grid("the list names the unit's branch", |g| {
        g.contains(INTERRUPTED_WORK_TITLE) && g.contains(&branch)
    });
    deck.send_keys(b"r");
    deck.wait_until_grid("the dry run's verdict and the y/n question", |g| {
        g.contains(RECLAIM_WOULD_REMOVE) && g.contains(RECLAIM_EXPLANATION) && g.contains("y/n")
    });
    assert!(worktree.is_dir(), "the dry run removes nothing");
    deck.send_keys(b"y");
    deck.wait_until_grid("the reclaim's sentence and the refreshed footer", |g| {
        g.contains(RECLAIM_REMOVED) && g.contains(&interrupted_footer(1))
    });
    assert!(
        !worktree.exists(),
        "reclaim removed the worktree {}",
        worktree.display()
    );
    assert!(
        branch_listed(&decks.repo, &branch),
        "reclaim keeps the unit's branch"
    );
    let after = decks.list();
    assert!(
        unit_item(&after, UNIT).is_none(),
        "a reclaimed item is not listed: {after}"
    );
}
