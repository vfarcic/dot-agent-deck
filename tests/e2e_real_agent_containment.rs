#![cfg(all(feature = "e2e", unix))]

//! What the harness's launches show an agent names no path in the developer's
//! checkout (PRD #1666) — checked on the launches themselves, credential-free,
//! so CI runs it on every PR.
//!
//! The deck spells its own absolute path into agent text, and a real agent
//! handed `<checkout>/target/debug/dot-agent-deck` was measured writing its work
//! into the checkout (`orchestration/dispatch/002`). Every harness launch
//! therefore runs the deck as `common::contained_deck_binary()`, a link outside
//! the checkout. `orchestration/dispatch/006` checks the dashboard launch through
//! the text a dispatched orchestration's agents are handed and the `PATH` they
//! inherit; `/009` checks the two daemon fixtures. `tests/real_agent_containment.rs`
//! holds the source guard (`/007`) and the link's own checks (`/008`).

mod common;
#[path = "support/inbox_view.rs"]
mod inbox_view;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

/// Removes a dispatch worktree on drop, including on panic: dispatch worktrees
/// are siblings of the fixture dir, outside the harness tempdir.
struct SiblingWorktreeGuard(PathBuf);

impl Drop for SiblingWorktreeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The sibling worktree a dispatch of `unit` creates — `../<repo>-dispatch-<unit>`.
fn dispatch_worktree_of(deck: &TuiDeck, unit: &str) -> PathBuf {
    deck.workdir()
        .parent()
        .expect("fixture dir has a parent")
        .join(format!(
            "{}-dispatch-{unit}",
            deck.workdir()
                .file_name()
                .expect("fixture dir has a name")
                .to_string_lossy()
        ))
}

/// Open one `cat` pane and return its `DOT_AGENT_DECK_PANE_ID`: a dispatch
/// resolves its working dir from the calling pane's record. The Command field is
/// cleared first, because the form seeds it from the config.
fn open_cat_caller_pane(deck: &TuiDeck) -> String {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm dir → new-pane form
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(b"caller");
    deck.send_keys(b"\t");
    deck.send_keys(&[0x7f; 96]);
    deck.send_keys(b"cat");
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");

    const PANE_WAIT: Duration = Duration::from_secs(60);
    let find_caller = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find_map(|r| r.pane_id_env.filter(|_| r.cwd.is_some()))
    };
    assert!(
        common::wait_until(PANE_WAIT, || find_caller().is_some()),
        "no registered pane with a cwd appeared within {}s.\nFinal grid:\n{}",
        PANE_WAIT.as_secs(),
        deck.snapshot_grid()
    );
    find_caller().expect("checked above")
}

/// Every role pane of orchestration `orch` running in a directory named
/// `basename`, as `role name → DOT_AGENT_DECK_PANE_ID`.
fn role_panes_in(socket: &Path, orch: &str, basename: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for record in common::agent_records_on(socket) {
        let Some(TabMembership::Orchestration {
            name, role_name, ..
        }) = record.tab_membership.clone()
        else {
            continue;
        };
        let (Some(cwd), Some(pane_id)) = (record.cwd.clone(), record.pane_id_env.clone()) else {
            continue;
        };
        if name == orch && Path::new(&cwd).file_name().is_some_and(|b| b == basename) {
            out.insert(role_name, pane_id);
        }
    }
    out
}

/// The `PATH` process `pid` runs with, read from `/proc`.
#[cfg(target_os = "linux")]
fn path_of_process(pid: u32) -> String {
    let environ = std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_else(|e| panic!("read /proc/{pid}/environ: {e}"));
    environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"PATH="))
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .unwrap_or_else(|| panic!("process {pid} runs with no PATH"))
}

/// Panic naming `what` if any entry of `path` sits in the developer's checkout.
#[cfg(target_os = "linux")]
fn assert_path_names_no_checkout(what: &str, path: &str) {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
    let canonical = checkout.canonicalize().ok();
    for entry in std::env::split_paths(path) {
        assert!(
            !entry.starts_with(checkout)
                && !canonical
                    .as_deref()
                    .is_some_and(|root| entry.starts_with(root)),
            "{what} names the developer's checkout ({}) — an agent that inspects its \
             PATH learns the checkout path. PATH: {path}",
            entry.display()
        );
    }
}

/// Scenario: Launch the deck on the `orch-deck` fixture the way every harness
/// launch runs it, dispatch its two-role orchestration through the real CLI and
/// delegate a task to the worker. The orchestrator's context file and the
/// worker's task, on disk and in its inbox, must name the deck by the harness's
/// contained link and never by any path inside the checkout the tests were built
/// in, and the `PATH` the deck hands down must hold no checkout path either.
#[spec("orchestration/dispatch/006")]
#[test]
fn orchestration_dispatch_006_agent_instructions_never_name_the_developer_checkout() {
    const UNIT: &str = "contain-probe";
    const TASK: &str = "Contained delegation.";
    const ORCH: &str = "demo-orch";
    let checkout = env!("CARGO_MANIFEST_DIR");

    // No opt-in: containment is every launch's default.
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_env("PATH", common::path_with_binary_dir())
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active agents");
    common::commit_fixture_repo(deck.workdir());
    let caller_pane = open_cat_caller_pane(&deck);
    let expected_worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(expected_worktree.clone());

    // The CLIs run as cargo's build artifact on purpose: they are the TEST's
    // calls, and an agent's text that named their path would be a leak this
    // test should see. Everything handed to an agent is composed deck-side.
    let cli = env!("CARGO_BIN_EXE_dot-agent-deck");
    let out = std::process::Command::new(cli)
        .args([
            "dispatch",
            UNIT,
            "--task",
            "Say hello, then stop.",
            "--orchestration=demo-orch",
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller_pane)
        .output()
        .expect("the dispatch CLI should run");
    assert!(
        out.status.success(),
        "`dispatch --orchestration demo-orch` failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    const WAIT: Duration = Duration::from_secs(90);
    let worktree_dir = expected_worktree
        .file_name()
        .expect("the dispatch worktree has a basename")
        .to_string_lossy()
        .into_owned();
    let socket = deck.attach_socket_path();
    let both_roles = || {
        let found = role_panes_in(socket, ORCH, &worktree_dir);
        found.contains_key("orchestrator") && found.contains_key("worker")
    };
    assert!(
        common::wait_until(WAIT, both_roles),
        "the dispatched `{ORCH}` never had both role panes within {}s — got {:?}.\nFinal grid:\n{}",
        WAIT.as_secs(),
        role_panes_in(socket, ORCH, &worktree_dir),
        deck.snapshot_grid()
    );
    let orchestrator = role_panes_in(socket, ORCH, &worktree_dir)["orchestrator"].clone();
    let out = std::process::Command::new(cli)
        .args(["delegate", "--to", "worker", "--task", TASK])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &orchestrator)
        .output()
        .expect("the delegate CLI should run");
    assert!(
        out.status.success(),
        "`delegate` from the dispatched orchestrator exited {:?}.\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Re-resolved on every poll: a delegate may replace the worker's pane.
    let inbox_task = || {
        let worker = role_panes_in(socket, ORCH, &worktree_dir)
            .get("worker")
            .cloned()?;
        inbox_view::messages(socket, &worker)
            .into_iter()
            .filter_map(|message| message.text)
            .find(|text| text.contains(TASK))
    };
    assert!(
        common::wait_until(WAIT, || inbox_task().is_some()),
        "the dispatched worker never received the delegated task within {}s.\nFinal grid:\n{}",
        WAIT.as_secs(),
        deck.snapshot_grid()
    );

    // What the agents are handed: the orchestrator's context, the worker's task
    // file, and the task as it sits in the worker's inbox.
    let coordination = expected_worktree.join(".dot-agent-deck");
    let context = std::fs::read_dir(&coordination)
        .expect("list the dispatched coordination files")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("orchestrator-context-") && name.ends_with(".md")
                })
        })
        .expect("the dispatched orchestration publishes a context file");
    let handed = [
        (
            "the orchestrator context",
            std::fs::read_to_string(&context).expect("read the orchestrator context"),
        ),
        (
            "the worker task file",
            std::fs::read_to_string(coordination.join("worker-task-worker.md"))
                .expect("read the worker task file"),
        ),
        (
            "the worker's inbox task",
            inbox_task().expect("checked above"),
        ),
    ];

    // The positive half keeps the containment claim from passing vacuously: the
    // deck does name itself in this text, by the harness's contained link.
    let contained = common::contained_deck_binary()
        .to_string_lossy()
        .into_owned();
    for (what, text) in &handed {
        assert!(
            !text.contains(checkout),
            "{what} names the developer's checkout {checkout} — a real agent handed this \
             text was measured writing its work into that checkout instead of its own \
             directory (orchestration/dispatch/002). Text:\n{text}"
        );
        assert!(
            text.contains(contained.as_str()),
            "{what} does not name the deck by its contained path {contained}, so the \
             containment above proves nothing. Text:\n{text}"
        );
    }

    // The other route to the checkout: the `PATH` the deck — and through it the
    // daemon and every agent — runs with. A lookup by name must land on the
    // contained directory. `/proc` is Linux's; macOS still gets the text half.
    #[cfg(target_os = "linux")]
    {
        let path = path_of_process(deck.child_pid().expect("the deck process has a pid"));
        assert_eq!(
            std::env::split_paths(&path).next().as_deref(),
            Some(common::contained_deck_path_dir()),
            "the build under test must win a lookup by name through the contained \
             directory. PATH: {path}"
        );
        assert_path_names_no_checkout("the deck's PATH", &path);
    }
}

/// Scenario: Start the harness's headless `daemon serve` fixture with a `PATH`
/// that carries a checkout directory, and its in-process daemon — the two
/// launches besides the dashboard that real-agent tests use. The headless daemon
/// must run as the contained link with that directory gone from its `PATH`, and
/// the in-process daemon must name the contained link wherever it spells the deck
/// into agent text.
#[spec("orchestration/dispatch/009")]
#[test]
fn orchestration_dispatch_009_every_harness_daemon_runs_contained() {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
    let contained = common::contained_deck_binary();

    let in_checkout = checkout.join("tests");
    let path = std::env::join_paths(
        std::iter::once(in_checkout.clone())
            .chain(std::env::split_paths(&common::path_with_binary_dir())),
    )
    .expect("join a PATH");
    let daemon = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[("PATH", path.to_str().expect("PATH is UTF-8"))],
    );
    #[cfg(target_os = "linux")]
    {
        let exe = std::fs::read_link(format!("/proc/{}/exe", daemon.pid()))
            .expect("read the daemon's executable");
        assert_eq!(
            exe.canonicalize().ok(),
            contained.canonicalize().ok(),
            "the headless daemon must run as the contained deck, not {}",
            exe.display()
        );
        let path = path_of_process(daemon.pid());
        assert_path_names_no_checkout("the headless daemon's PATH", &path);
        assert!(
            std::env::split_paths(&path).any(|entry| entry == common::contained_deck_path_dir()),
            "the headless daemon's PATH lost the contained directory: {path}"
        );
    }
    drop(daemon);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime");
    runtime.block_on(async {
        let _daemon = common::spawn_inprocess_daemon().await;
        let named = dot_agent_deck::platform::paths::binary_name();
        assert!(
            !Path::new(&named).starts_with(checkout),
            "the in-process daemon names the deck as {named}, inside the checkout"
        );
        assert_eq!(
            named,
            contained.to_string_lossy(),
            "the in-process daemon must name the deck by the contained link"
        );
    });
}
