#![cfg(all(feature = "e2e", unix))]

//! PTY-attached coverage for issue #1602: a `dispatch --single` unit is
//! started with the command the dispatching pane was configured with, and as
//! that pane's agent, instead of the deck's `default_command`.
//!
//! Both commands are token-free shell stand-ins. The caller's plays the part of
//! `devbox run agent`: a launcher whose name reveals no agent, which says what
//! it runs only through its own hook.

dot_agent_deck_test_harness::link!();

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::event::AgentType;
use spec::spec;

const LAUNCHER_READY: &str = "LAUNCHER-STAND-IN-READY";
/// Untracked, so it exists in the caller's checkout and not in the unit's
/// worktree (a HEAD checkout). It is what lets the caller announce itself as
/// Claude Code while the unit stays silent — so the unit's type can only have
/// come from its dispatcher.
const ANNOUNCE_MARKER: &str = ".announce-as-claude";

/// Removes a dispatch worktree on drop, including while a RED assertion is
/// unwinding. Dispatch worktrees are siblings of the harness tempdir, so the
/// harness cannot reclaim them itself.
struct SiblingWorktreeGuard(PathBuf);

impl Drop for SiblingWorktreeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Write the two stand-ins and a deck config naming the DEFAULT one, and
/// return `(config, launch log, launcher command)`.
///
/// Each stand-in appends `<which>|<cwd>` to the launch log when it starts, so
/// the log records which command ran where.
fn write_stand_ins(dir: &Path) -> (PathBuf, PathBuf, String) {
    let log = dir.join("launch.log");
    let quoted_log = shell_quote(&log.to_string_lossy());
    let quoted_bin = shell_quote(env!("CARGO_BIN_EXE_dot-agent-deck"));

    let default_probe = dir.join("default-probe.sh");
    std::fs::write(
        &default_probe,
        format!(
            "#!/bin/sh\n\
             printf 'default|%s\\n' \"$PWD\" >> {quoted_log}\n\
             exec cat\n"
        ),
    )
    .expect("write the default-command stand-in");

    let launcher = dir.join("launcher.sh");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\n\
             printf 'launcher|%s\\n' \"$PWD\" >> {quoted_log}\n\
             if [ -f {ANNOUNCE_MARKER} ]; then\n\
             \x20 printf '{{\"hook_event_name\":\"SessionStart\",\"session_id\":\"launcher-%s\"}}' \
             \"$DOT_AGENT_DECK_PANE_ID\" | {quoted_bin} hook --agent claude-code >/dev/null 2>&1\n\
             fi\n\
             printf '{LAUNCHER_READY}\\r\\n'\n\
             exec cat\n"
        ),
    )
    .expect("write the launcher stand-in");

    let default_command = format!("sh {}", default_probe.display());
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "default_command = \"{}\"\n",
            default_command.replace('\\', "\\\\").replace('"', "\\\"")
        ),
    )
    .expect("write the deck config");

    (config, log, format!("sh {}", launcher.display()))
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

fn records_summary(deck: &TuiDeck) -> Vec<(String, Option<String>, Option<String>, String)> {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .map(|record| {
            (
                record.id,
                record.display_name,
                record.cwd,
                format!("{:?}", record.agent_type),
            )
        })
        .collect()
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Open a dashboard pane named `caller` whose Command is `command`, replacing
/// the `default_command` the form opens pre-filled with.
fn open_caller(deck: &TuiDeck, command: &str) -> dot_agent_deck::agent_pty::AgentRecord {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> new-pane form
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t"); // Mode -> Name
    deck.send_keys(&[0x7f; 96]); // clear the cwd-derived default name
    deck.send_keys(b"caller");
    deck.send_keys(b"\t"); // Name -> Command
    deck.send_keys(&[0x7f; 400]); // clear the pre-filled default_command
    deck.send_keys(command.as_bytes());
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");

    let find = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|record| record.display_name.as_deref() == Some("caller"))
    };
    const PANE_WAIT: Duration = Duration::from_secs(60);
    // Ready, AND announced: the caller's type comes only from its own hook, the
    // way `devbox run agent`'s does.
    assert!(
        common::wait_until(PANE_WAIT, || {
            find().is_some_and(|record| {
                record.agent_type == Some(AgentType::ClaudeCode)
                    && common::pane_search_key_on(deck.attach_socket_path(), &record.id)
                        .contains(LAUNCHER_READY)
            })
        }),
        "the caller did not start, print {LAUNCHER_READY:?} and announce itself as Claude Code \
         within {}s.\nRecords: {:?}\nFinal grid:\n{}",
        PANE_WAIT.as_secs(),
        records_summary(deck),
        deck.snapshot_grid()
    );
    find().expect("the readiness poll found the caller")
}

fn run_dispatch(deck: &TuiDeck, caller_pane_id: &str, unit: &str) -> Output {
    run_dispatch_with(deck, caller_pane_id, unit, &[])
}

fn run_dispatch_with(deck: &TuiDeck, caller_pane_id: &str, unit: &str, extra: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "dispatch",
            unit,
            "--task",
            "Report which command started you.",
            "--single",
        ])
        .args(extra)
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", caller_pane_id)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run the real dispatch CLI")
}

/// Scenario: Start a dashboard pane whose command is a launcher stand-in that
/// reveals no agent and announces itself as Claude Code through its own hook,
/// beside a deck whose `default_command` is a different stand-in. Dispatch a
/// `--single` unit from that pane: the unit must run the launcher, in its own
/// worktree, registered as Claude Code, and the default command must not run.
#[spec("dispatch/single/001")]
#[test]
fn dispatch_single_001_unit_runs_the_dispatchers_command_as_its_agent() {
    const UNIT: &str = "single-launch-probe";

    let scratch = common::race_safe_tempdir();
    let (config, log, launcher_command) = write_stand_ins(scratch.path());
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    common::commit_fixture_repo(deck.workdir());
    // After the commit, so the unit's worktree does not get it.
    std::fs::write(deck.workdir().join(ANNOUNCE_MARKER), "").expect("write announce marker");

    let caller = open_caller(&deck, &launcher_command);
    let caller_pane_id = caller
        .pane_id_env
        .clone()
        .expect("the caller pane carries a pane id");
    assert!(
        read_log(&log).lines().any(|l| l.starts_with("launcher|")),
        "precondition: the caller itself ran the launcher.\nLaunch log:\n{}",
        read_log(&log)
    );

    let worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(worktree.clone());
    let dispatched = run_dispatch(&deck, &caller_pane_id, UNIT);
    assert!(
        dispatched.status.success(),
        "dispatch --single exited {:?}\nstdout: {}\nstderr: {}",
        dispatched.status.code(),
        String::from_utf8_lossy(&dispatched.stdout),
        String::from_utf8_lossy(&dispatched.stderr)
    );

    let worktree_name = worktree
        .file_name()
        .expect("worktree has a basename")
        .to_string_lossy()
        .into_owned();
    let in_worktree = |line: &str, which: &str| {
        line.strip_prefix(which)
            .and_then(|cwd| {
                Path::new(cwd)
                    .file_name()
                    .map(|n| n == worktree_name.as_str())
            })
            .unwrap_or(false)
    };
    const UNIT_WAIT: Duration = Duration::from_secs(30);
    let started = common::wait_until(UNIT_WAIT, || {
        read_log(&log)
            .lines()
            .any(|l| in_worktree(l, "launcher|") || in_worktree(l, "default|"))
    });
    let launch_log = read_log(&log);
    assert!(
        started,
        "no command started in the unit's worktree {} within {}s.\nLaunch log:\n{launch_log}\n\
         Records: {:?}\nFinal grid:\n{}",
        worktree.display(),
        UNIT_WAIT.as_secs(),
        records_summary(&deck),
        deck.snapshot_grid()
    );
    assert!(
        launch_log.lines().any(|l| in_worktree(l, "launcher|")),
        "the --single unit must run the command its dispatcher was started with, in its own \
         worktree ({worktree_name}). Before issue #1602 it ran the deck's default_command \
         instead.\nLaunch log:\n{launch_log}"
    );
    assert!(
        !launch_log.lines().any(|l| l.starts_with("default|")),
        "the deck's default_command must not run when the dispatcher has a command of its own.\n\
         Launch log:\n{launch_log}"
    );

    // The unit never announces (its worktree has no marker), so a Claude Code
    // type on its record can only be the one carried from the dispatcher.
    let unit = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| {
            record.display_name.as_deref() == Some(format!("dispatch-{UNIT}").as_str())
                && record
                    .cwd
                    .as_deref()
                    .and_then(|cwd| {
                        Path::new(cwd)
                            .file_name()
                            .map(|n| n == worktree_name.as_str())
                    })
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            panic!(
                "the unit's record was not found.\nRecords: {:?}",
                records_summary(&deck)
            )
        });
    assert_eq!(
        unit.agent_type,
        Some(AgentType::ClaudeCode),
        "the unit must be registered as its dispatcher's agent type, which its launcher \
         command cannot reveal.\nRecords: {:?}",
        records_summary(&deck)
    );
}

const CHOSEN_READY: &str = "CHOSEN-STAND-IN-READY";

/// Issue #2172: the command a dispatch chooses for its unit, standing in for
/// `devbox run oc-big`: a launcher whose name reveals no agent and which never
/// announces one, so the unit's type can only come from `--agent`. Records
/// `chosen|<cwd>` in the same launch log as the other two stand-ins.
fn write_chosen_stand_in(dir: &Path, log: &Path) -> String {
    let chosen = dir.join("chosen.sh");
    std::fs::write(
        &chosen,
        format!(
            "#!/bin/sh\n\
             printf 'chosen|%s\\n' \"$PWD\" >> {log}\n\
             printf '{CHOSEN_READY}\\r\\n'\n\
             exec cat\n",
            log = shell_quote(&log.to_string_lossy()),
        ),
    )
    .expect("write the chosen stand-in");
    format!("sh {}", chosen.display())
}

/// Scenario: Start a dashboard pane whose command is a launcher stand-in that
/// announces itself as Claude Code, beside a deck whose `default_command` is a
/// different stand-in. Dispatch a unit from that pane with `--command` naming
/// a third stand-in and `--agent opencode`: the unit's pane must run the
/// chosen command in its own worktree, registered as OpenCode, and neither the
/// dispatcher's command nor the default command may start there.
#[spec("dispatch/single/002")]
#[test]
fn dispatch_single_002_a_chosen_command_runs_as_the_chosen_agent() {
    const UNIT: &str = "single-chosen-probe";

    let scratch = common::race_safe_tempdir();
    let (config, log, launcher_command) = write_stand_ins(scratch.path());
    let chosen_command = write_chosen_stand_in(scratch.path(), &log);
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_env("PATH", common::path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    common::commit_fixture_repo(deck.workdir());
    std::fs::write(deck.workdir().join(ANNOUNCE_MARKER), "").expect("write announce marker");

    let caller = open_caller(&deck, &launcher_command);
    let caller_pane_id = caller
        .pane_id_env
        .clone()
        .expect("the caller pane carries a pane id");

    let worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(worktree.clone());
    let dispatched = run_dispatch_with(
        &deck,
        &caller_pane_id,
        UNIT,
        &["--command", &chosen_command, "--agent", "opencode"],
    );
    assert!(
        dispatched.status.success(),
        "dispatch --command --agent exited {:?}\nstdout: {}\nstderr: {}",
        dispatched.status.code(),
        String::from_utf8_lossy(&dispatched.stdout),
        String::from_utf8_lossy(&dispatched.stderr)
    );

    let worktree_name = worktree
        .file_name()
        .expect("worktree has a basename")
        .to_string_lossy()
        .into_owned();
    let in_worktree = |cwd: &str| {
        Path::new(cwd)
            .file_name()
            .is_some_and(|n| n == worktree_name.as_str())
    };
    let find_unit = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|record| {
                record.display_name.as_deref() == Some(format!("dispatch-{UNIT}").as_str())
                    && record.cwd.as_deref().is_some_and(in_worktree)
            })
    };
    const UNIT_WAIT: Duration = Duration::from_secs(30);
    // The unit's own pane shows the chosen command's output.
    let ready = common::wait_until(UNIT_WAIT, || {
        find_unit().is_some_and(|record| {
            common::pane_search_key_on(deck.attach_socket_path(), &record.id).contains(CHOSEN_READY)
        })
    });
    let launch_log = read_log(&log);
    assert!(
        ready,
        "the unit's pane did not show {CHOSEN_READY:?} within {}s, so it is not running the \
         command --command chose.\nLaunch log:\n{launch_log}\nRecords: {:?}\nFinal grid:\n{}",
        UNIT_WAIT.as_secs(),
        records_summary(&deck),
        deck.snapshot_grid()
    );
    let started_in_worktree = |which: &str| {
        launch_log
            .lines()
            .any(|l| l.strip_prefix(which).is_some_and(in_worktree))
    };
    assert!(
        started_in_worktree("chosen|"),
        "the chosen command must start in the unit's worktree ({worktree_name}).\n\
         Launch log:\n{launch_log}"
    );
    assert!(
        !started_in_worktree("launcher|"),
        "the dispatcher's own command must not start in the unit when --command chose \
         another.\nLaunch log:\n{launch_log}"
    );
    assert!(
        !launch_log.lines().any(|l| l.starts_with("default|")),
        "the deck's default_command must not run.\nLaunch log:\n{launch_log}"
    );
    // The chosen stand-in never announces, and the dispatcher is Claude Code,
    // so OpenCode on the unit's record can only have come from --agent.
    assert_eq!(
        find_unit().and_then(|record| record.agent_type),
        Some(AgentType::OpenCode),
        "the unit must be registered as the agent --agent named.\nRecords: {:?}",
        records_summary(&deck)
    );
}

/// What an older-daemon stand-in read: every line, by connection.
type ReadLines = std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>;

/// A hook socket that answers the capabilities question the way v0.49's
/// daemon does, without `dispatch_launch`, acknowledges any `dispatch` the way
/// every daemon since issue #1129 does, and records what it read.
fn older_daemon_stand_in(path: &Path) -> ReadLines {
    use std::io::{BufRead as _, Write as _};
    let listener = std::os::unix::net::UnixListener::bind(path).expect("bind the stand-in");
    let lines: ReadLines = ReadLines::default();
    let seen = std::sync::Arc::clone(&lines);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let reader = std::io::BufReader::new(stream.try_clone().expect("clone the stream"));
            let index = {
                let mut seen = seen.lock().unwrap();
                seen.push(Vec::new());
                seen.len() - 1
            };
            for line in reader.lines() {
                let Ok(line) = line else { break };
                seen.lock().unwrap()[index].push(line.clone());
                let answer = if line.contains(r#""message_type":"hook_capabilities""#) {
                    r#"{"task_scoped_completion":1,"input_mode_report":1}"#
                } else {
                    r#"{"kind":"signal_ack","accepted":true}"#
                };
                if writeln!(stream, "{answer}").is_err() {
                    break;
                }
            }
        }
    });
    lines
}

fn dispatch_against(socket: &Path, cwd: &Path, extra: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["dispatch", "older-daemon-probe", "--task", "t"])
        .args(extra)
        .env("DOT_AGENT_DECK_SOCKET", socket)
        .env("DOT_AGENT_DECK_PANE_ID", "pane-older-daemon")
        .env("HOME", cwd)
        .current_dir(cwd)
        .output()
        .expect("run the real dispatch CLI")
}

/// Scenario: Point the real `dispatch` CLI at a hook socket that answers like
/// an older daemon, one that does not know a dispatch can choose its command
/// or agent. A dispatch with `--command` and `--agent` must exit non-zero,
/// say nothing was dispatched because the daemon is too old for them, and send
/// that daemon no dispatch at all; the same dispatch without them is sent and
/// succeeds exactly as before.
#[spec("dispatch/single/003")]
#[test]
fn dispatch_single_003_an_older_daemon_gets_no_dispatch_that_chose_its_command() {
    let scratch = common::race_safe_tempdir();
    let socket = scratch.path().join("older-hook.sock");
    let read = older_daemon_stand_in(&socket);
    let dispatches = || -> Vec<String> {
        read.lock()
            .unwrap()
            .iter()
            .flatten()
            .filter(|line| line.contains(r#""message_type":"dispatch""#))
            .cloned()
            .collect()
    };

    for chosen in [
        &["--command", "devbox run oc-big", "--agent", "opencode"][..],
        &["--agent", "opencode"][..],
        &["--command", "opencode"][..],
    ] {
        let refused = dispatch_against(&socket, scratch.path(), chosen);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            !refused.status.success(),
            "{chosen:?} against an older daemon must fail.\nstderr: {stderr}"
        );
        assert!(
            stderr.contains("nothing was dispatched") && stderr.contains("--command"),
            "{chosen:?}: the refusal must say nothing was dispatched and why.\nstderr: {stderr}"
        );
        assert!(
            dispatches().is_empty(),
            "{chosen:?}: an older daemon must not receive the dispatch, which it would \
             start with the dispatcher's own command.\nRead: {:?}",
            read.lock().unwrap()
        );
    }

    // Control: without the flags, the dispatch goes out as it always did.
    let plain = dispatch_against(&socket, scratch.path(), &["--single"]);
    assert!(
        plain.status.success(),
        "a dispatch without --command or --agent is unchanged.\nstderr: {}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let sent = dispatches();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        !sent[0].contains(r#""command":"#) && !sent[0].contains(r#""agent":"#),
        "a dispatch that chose nothing carries no launch keys: {}",
        sent[0]
    );
}
