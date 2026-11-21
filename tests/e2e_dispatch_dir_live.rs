#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Real-agent coverage for PRD #468: a single agent that has finished an
//! analysis hands the work to an orchestration in its OWN directory, with
//! `dispatch --dir .`, referencing a brief it wrote there by path.
//!
//! Lane 2: a real interactive Haiku agent on the developer's own credentials
//! (CLAUDE.md rule 5) does the half only an agent can do — write the brief and
//! run the dispatch itself. The orchestration it starts is a stand-in team
//! (`cat` roles), so what the test reads is where that team came up and what
//! its orchestrator was handed.

dot_agent_deck_test_harness::link!();

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::event::AgentType;
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const UNIT: &str = "dir-handoff";
const TEAM: &str = "continue-team";
const BRIEF: &str = ".dot-agent-deck/handoff-468-7b3e.md";
const BRIEF_WORD: &str = "PELICAN-468-HANDOFF";

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The first `claude` on `PATH` this user can execute, so the launcher execs
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

/// An interactive Haiku Claude Code with the tools the turn needs.
fn write_launcher(dir: &Path) -> String {
    let script = dir.join("agent-launcher.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec {claude} --ax-screen-reader --model {HAIKU_MODEL} \
             --allowedTools Bash Read Write\n",
            claude = shell_quote(&real_claude_path().to_string_lossy()),
        ),
    )
    .expect("write the launcher");
    format!("sh {}", script.display())
}

/// Every role of the dispatched team, rooted in `root`.
fn team_roles(deck: &TuiDeck, root: &Path) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .filter(|r| {
            matches!(&r.tab_membership, Some(TabMembership::Orchestration { name, .. }) if name == TEAM)
        })
        .filter(|r| {
            r.cwd
                .as_deref()
                .is_some_and(|cwd| std::fs::canonicalize(cwd).is_ok_and(|cwd| cwd == root))
        })
        .collect()
}

/// Scenario: Open a real interactive Haiku agent as a plain pane in a checkout
/// whose config offers a stand-in team. Tell it to write a brief into
/// `.dot-agent-deck/` and then hand the work to that team in its own directory
/// with `dispatch --dir . --orchestration`, giving the brief as the task file.
/// The brief must exist where the agent wrote it, both roles of the team must
/// come up as an orchestration rooted in the agent's own directory, the
/// orchestrator's context must carry the brief's text, and no `-dispatch-`
/// sibling worktree may be created.
#[spec("dispatch/dir/003")]
#[test]
fn dispatch_dir_003_a_real_agent_hands_its_work_to_a_team_in_its_own_directory() {
    skip_unless!(common::check_claude_available());

    let scratch = common::race_safe_tempdir();
    let launcher = write_launcher(scratch.path());
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(220, 60)
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "420")
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        // The agent's `dot-agent-deck dispatch` must be the branch build.
        .with_env("PATH", common::path_with_binary_dir())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let workdir = deck.workdir().to_path_buf();
    std::fs::write(
        workdir.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\nname = \"{TEAM}\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n\n\
             [[orchestrations.roles]]\nname = \"worker\"\ncommand = \"cat\"\n\
             description = \"Implements what the brief concludes\"\n"
        ),
    )
    .expect("write the stand-in team");
    // PRD #1885: the team is approved, as a user's is before they rely on it.
    deck.approve_project_trust(&workdir);
    common::commit_fixture_repo(&workdir);
    let sibling = workdir.parent().expect("fixture parent").join(format!(
        "{}-dispatch-{UNIT}",
        workdir.file_name().expect("fixture name").to_string_lossy()
    ));

    // A plain agent pane, as a user opens one, running the Haiku launcher.
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // Space -> current dir -> new-pane form
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t"); // Mode -> Name
    deck.send_keys(&[0x7f; 96]);
    deck.send_keys(b"analyst");
    deck.send_keys(b"\t"); // Name -> Command
    deck.send_keys(&[0x7f; 400]);
    deck.send_keys(launcher.as_bytes());
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");

    const READY_WAIT: Duration = Duration::from_secs(120);
    let analyst = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|r| r.display_name.as_deref() == Some("analyst"))
    };
    assert!(
        common::wait_until(READY_WAIT, || analyst()
            .is_some_and(|r| r.agent_type == Some(AgentType::ClaudeCode))),
        "a real Haiku agent never came up within {}s.\nFinal grid:\n{}",
        READY_WAIT.as_secs(),
        deck.snapshot_grid()
    );
    let analyst_id = analyst().expect("checked above").id;
    if !common::wait_until_panes_settled(
        deck.attach_socket_path(),
        std::slice::from_ref(&analyst_id),
        Duration::from_millis(1500),
        Duration::from_secs(8),
        Duration::from_secs(90),
    ) {
        eprintln!("warning: the agent pane did not settle within 90s; proceeding anyway");
    }

    let directive = format!(
        "Use your Write tool to create the file {BRIEF} in the current directory, containing \
         exactly this one line: Continue the work. Marker {BRIEF_WORD}. Then use Bash to run \
         exactly this command and wait for it: dot-agent-deck dispatch {UNIT} --dir . \
         --orchestration={TEAM} --task-file {BRIEF} \
         Do not do anything else, and do not ask me anything first."
    );
    deck.send_keys(directive.as_bytes());
    deck.send_keys(b"\r");

    let root = std::fs::canonicalize(&workdir).expect("canonical workdir");
    const WORK_WAIT: Duration = Duration::from_secs(300);
    let came_up = common::wait_until(WORK_WAIT, || team_roles(&deck, &root).len() == 2);
    assert!(
        came_up,
        "the agent's dispatch never brought both roles of `{TEAM}` up in its own directory {} \
         within {}s.\nBrief present: {}\nRecords: {:?}\nFinal grid:\n{}",
        root.display(),
        WORK_WAIT.as_secs(),
        workdir.join(BRIEF).is_file(),
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|r| (
                r.display_name.clone(),
                r.cwd.clone(),
                r.tab_membership.clone()
            ))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    let brief = std::fs::read_to_string(workdir.join(BRIEF)).unwrap_or_default();
    assert!(
        brief.contains(BRIEF_WORD),
        "the brief must be where the agent wrote it, holding its text: {brief:?}"
    );
    let context_carries_the_brief = || {
        std::fs::read_dir(workdir.join(".dot-agent-deck"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("orchestrator-context-"))
            })
            .any(|e| std::fs::read_to_string(e.path()).is_ok_and(|t| t.contains(BRIEF_WORD)))
    };
    assert!(
        common::wait_until(Duration::from_secs(30), context_carries_the_brief),
        "the orchestrator's context in the agent's own directory must carry the brief's text"
    );
    assert!(
        common::wait_until(Duration::from_secs(30), || deck
            .snapshot_grid()
            .contains("Dashboard")),
        "the team never became a tab on the deck.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        !sibling.exists(),
        "a --dir dispatch must not create a sibling worktree at {}",
        sibling.display()
    );
    assert!(
        analyst().is_some(),
        "the agent that handed over is still running beside its team"
    );
}
