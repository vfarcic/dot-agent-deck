#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Real-agent coverage for issue #2172: a dispatcher asked to start a unit with
//! a chosen command and agent (`dispatch --command … --agent …`) gets a unit
//! that runs that command, as that agent, and does its task.
//!
//! Lane 2: a real interactive Haiku dispatcher and a real interactive Haiku
//! unit, on the developer's own credentials (CLAUDE.md rule 5). The two are the
//! same agent, because that is the one agent this harness can run headless; what
//! tells them apart is the command each is started with, and each command is a
//! launcher that records where it started before it execs `claude`. The unit's
//! launcher hides its agent, so `--agent claude` is what gives the unit Claude
//! Code's prompt-delivery path, and a task that arrives and is done is the proof
//! that it did.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_view.rs"]
mod inbox_view;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::AgentType;
use spec::spec;

const DISPATCHER_SEED_OPENING: &str = "You are an ordinary assistant";
const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const SENTINEL: &str = "chosen-launch-live-sentinel-9d41.txt";

/// Removes a dispatch worktree on drop, including while a RED assertion is
/// unwinding. Dispatch worktrees are siblings of the harness tempdir, so the
/// harness cannot reclaim them itself.
struct SiblingWorktreeGuard(PathBuf);

impl Drop for SiblingWorktreeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The first `claude` on `PATH` this user can execute, so the launchers exec
/// the same agent `check_claude_available` found.
fn real_claude_path() -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("claude"))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o100 != 0)
        })
        .expect("`claude` on PATH — `check_claude_available` already passed")
}

/// A launcher that records `<which>|<cwd>` in `log` and then execs an
/// interactive Haiku Claude Code with the tools the two short turns need. Its
/// name reveals no agent, as `devbox run <script>` does not.
fn write_launcher(dir: &Path, which: &str, log: &Path) -> String {
    let script = dir.join(format!("{which}-launcher.sh"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             printf '{which}|%s\\n' \"$PWD\" >> {log}\n\
             exec {claude} --ax-screen-reader --model {HAIKU_MODEL} \
             --allowedTools Bash Read Write\n",
            log = shell_quote(&log.to_string_lossy()),
            claude = shell_quote(&real_claude_path().to_string_lossy()),
        ),
    )
    .expect("write a launcher");
    format!("sh {}", script.display())
}

fn seed_is_acknowledged(deck: &TuiDeck) -> bool {
    inbox_view::all_messages(deck.attach_socket_path())
        .into_iter()
        .any(|message| {
            message.state == dot_agent_deck::inbox::StateName::Acknowledged
                && message
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains(DISPATCHER_SEED_OPENING))
        })
}

/// Scenario: Open a real interactive Haiku dispatcher whose command is a
/// launcher, and tell it to dispatch a unit with `--command` naming a second
/// launcher that hides its agent and `--agent claude`, giving the unit a task
/// that writes a uniquely named sentinel file. The second launcher must start
/// in the unit's worktree and the dispatcher's must not, the unit must be
/// registered as Claude Code, and the real unit must write the sentinel there.
#[spec("dispatch/single/004")]
#[test]
fn dispatch_single_004_a_real_unit_runs_the_chosen_command_and_does_its_task() {
    skip_unless!(common::check_claude_available());
    const UNIT: &str = "chosen-live-probe";

    let scratch = common::race_safe_tempdir();
    let log = scratch.path().join("launch.log");
    let dispatcher_command = write_launcher(scratch.path(), "dispatcher", &log);
    let unit_command = write_launcher(scratch.path(), "unit", &log);
    let config = scratch.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "default_command = \"{}\"\n",
            dispatcher_command
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        ),
    )
    .expect("write the deck config");

    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(220, 60)
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "420")
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        // The dispatcher's `dot-agent-deck dispatch` must be the branch build.
        .with_env("PATH", common::path_with_binary_dir())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    common::commit_fixture_repo(deck.workdir());

    let worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(worktree.clone());
    // Trust the unit's worktree before it exists, under both spellings of a
    // macOS tempdir, so the unit's task reaches a usable agent rather than a
    // project-trust dialog.
    let mut trusted = vec![worktree.to_string_lossy().into_owned()];
    if let (Some(parent), Some(name)) = (worktree.parent(), worktree.file_name())
        && let Ok(canonical_parent) = parent.canonicalize()
    {
        let canonical = canonical_parent.join(name).to_string_lossy().into_owned();
        if !trusted.contains(&canonical) {
            trusted.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trusted)
        .expect("seed Claude trust for the unit's worktree");

    // A dispatcher pane through the new-pane form, as a user opens one.
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // Space -> current dir -> new-pane form
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C");
    deck.wait_for_string("dispatcher mode");
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");

    const DISPATCHER_READY_WAIT: Duration = Duration::from_secs(120);
    assert!(
        common::wait_until(DISPATCHER_READY_WAIT, || {
            deck.snapshot_grid().contains("ClaudeCode") && seed_is_acknowledged(&deck)
        }),
        "a real seeded dispatcher never became ready within {}s.\nFinal grid:\n{}",
        DISPATCHER_READY_WAIT.as_secs(),
        deck.snapshot_grid()
    );
    // Let the seed turn finish painting before typing the directive, as
    // `dispatch/return/006` does. Non-fatal: the seed's acknowledgement above
    // is the hard fact, and a busy host can keep a spinner repainting.
    let dispatcher_ids: Vec<String> = inbox_view::all_messages(deck.attach_socket_path())
        .into_iter()
        .filter(|message| {
            message
                .text
                .as_deref()
                .is_some_and(|text| text.contains(DISPATCHER_SEED_OPENING))
        })
        .filter_map(|message| message.bind_agent)
        .collect();
    // An empty list would make the settle-wait pass at once, typing the
    // directive into a pane still painting its seed turn with no warning
    // (Qodo, PR #2176). The seed was acknowledged above, so its agent is known.
    assert!(
        !dispatcher_ids.is_empty(),
        "precondition: the acknowledged dispatcher seed names no bound agent, so the test \
         cannot wait for the dispatcher pane to settle"
    );
    if !common::wait_until_panes_settled(
        deck.attach_socket_path(),
        &dispatcher_ids,
        Duration::from_millis(1500),
        Duration::from_secs(8),
        Duration::from_secs(90),
    ) {
        eprintln!("warning: the dispatcher pane did not settle within 90s; proceeding anyway");
    }

    // The dispatcher, not this test, runs the real CLI.
    let directive = format!(
        "Use Bash to run exactly this command now, then wait for its result: \
         dot-agent-deck dispatch {UNIT} --single --command '{unit_command}' --agent claude \
         --task \"Use Bash to run pwd, then create a file named {SENTINEL} in that directory \
         containing the word done. Do not ask questions.\" \
         Do not do the unit's work yourself, and do not ask me anything first."
    );
    deck.send_keys(directive.as_bytes());
    deck.send_keys(b"\r");

    let in_worktree = |cwd: &str| {
        Path::new(cwd)
            .file_name()
            .is_some_and(|n| Some(n) == worktree.file_name())
    };
    const WORK_WAIT: Duration = Duration::from_secs(300);
    let sentinel = worktree.join(SENTINEL);
    let done = common::wait_until(WORK_WAIT, || sentinel.is_file());
    let launch_log = std::fs::read_to_string(&log).unwrap_or_default();
    let records: Vec<_> = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .map(|r| (r.display_name, r.cwd, r.agent_type))
        .collect();
    assert!(
        done,
        "the unit never wrote {SENTINEL} in {} within {}s.\nLaunch log:\n{launch_log}\n\
         Records: {records:?}\nFinal grid:\n{}",
        worktree.display(),
        WORK_WAIT.as_secs(),
        deck.snapshot_grid()
    );
    let started_in_worktree = |which: &str| {
        launch_log
            .lines()
            .any(|l| l.strip_prefix(which).is_some_and(in_worktree))
    };
    assert!(
        started_in_worktree("unit|"),
        "the chosen command must start in the unit's worktree.\nLaunch log:\n{launch_log}"
    );
    assert!(
        !started_in_worktree("dispatcher|"),
        "the dispatcher's own command must not start in the unit.\nLaunch log:\n{launch_log}"
    );
    assert!(
        records.iter().any(|(name, cwd, agent_type)| {
            name.as_deref() == Some(format!("dispatch-{UNIT}").as_str())
                && cwd.as_deref().is_some_and(in_worktree)
                && *agent_type == Some(AgentType::ClaudeCode)
        }),
        "the unit must be registered as the agent --agent named.\nRecords: {records:?}"
    );
}
