#![cfg(feature = "e2e")]

//! PTY-attached coverage for PRD #220's dispatch return edge.
//!
//! Every scenario drives the real `dispatch` and `work-done` CLIs through the
//! deck's hook socket. The caller is a scripted, token-free terminal probe that
//! distinguishes a submitted CR from a passive LF, so seeing a report also
//! proves it arrived as a turn rather than as an inert notice.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

const PROBE_READY: &str = "DISPATCH-RETURN-PROBE-READY";
const RETURN_WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct PaneRef {
    pane_id: String,
    agent_id: String,
}

/// Removes a dispatch worktree on drop, including while a RED assertion is
/// unwinding. Dispatch worktrees are siblings of the harness tempdir, so the
/// harness cannot reclaim them itself.
struct SiblingWorktreeGuard(PathBuf);

impl Drop for SiblingWorktreeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bindir = Path::new(bin).parent().expect("binary path has a parent");
    format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Write a token-free pane command that makes delivery mode observable.
///
/// The probe puts stdin in raw mode, accumulates payload bytes, and prints a
/// line only when the daemon terminates them. CR is the guarded submit path;
/// LF is the passive notice path. A plain `cat` cannot distinguish those tails.
fn write_submit_probe(dir: &Path) -> (PathBuf, PathBuf) {
    let probe = dir.join("dispatch-return-probe.py");
    std::fs::write(
        &probe,
        r#"import os
import tty

tty.setraw(0)
os.write(1, b"DISPATCH-RETURN-PROBE-READY\r\n")
pending = bytearray()
while True:
    byte = os.read(0, 1)
    if not byte:
        break
    if byte == b"\r":
        os.write(1, b"SUBMITTED:" + bytes(pending) + b"\r\n")
        pending.clear()
    elif byte == b"\n":
        os.write(1, b"NOTICE:" + bytes(pending) + b"\r\n")
        pending.clear()
    else:
        pending.extend(byte)
"#,
    )
    .expect("write dispatch-return pane probe");

    let command = format!("python3 -u {}", probe.display());
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    let config = dir.join("config.toml");
    std::fs::write(&config, format!("default_command = \"{escaped}\"\n"))
        .expect("write probe default-command config");

    (config, dir.join("daemon.log"))
}

fn commit_fixture_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    };
    run(&["config", "user.email", "deck-test@example.com"]);
    run(&["config", "user.name", "Deck Test"]);
    run(&["add", "-A"]);
    run(&["commit", "-qm", "fixture baseline"]);
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

fn open_probe_caller(deck: &TuiDeck) -> PaneRef {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> new-pane form
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t"); // Mode -> Name
    deck.send_keys(&[0x7f; 96]); // clear the cwd-derived default name
    deck.send_keys(b"caller");
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");

    let find = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|record| record.display_name.as_deref() == Some("caller"))
            .and_then(|record| {
                Some(PaneRef {
                    pane_id: record.pane_id_env?,
                    agent_id: record.id,
                })
            })
    };
    const PANE_WAIT: Duration = Duration::from_secs(60);
    assert!(
        common::wait_until(PANE_WAIT, || {
            find().is_some_and(|pane| {
                common::pane_search_key_on(deck.attach_socket_path(), &pane.agent_id)
                    .contains(PROBE_READY)
            })
        }),
        "the caller probe did not register and print {PROBE_READY:?} within {}s.\n\
         Records: {:?}\nFinal grid:\n{}",
        PANE_WAIT.as_secs(),
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|record| (
                record.id.clone(),
                record.pane_id_env.clone(),
                record.display_name.clone()
            ))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    find().expect("the readiness poll found the caller")
}

fn run_dispatch(deck: &TuiDeck, caller: &PaneRef, unit: &str, shape: &str) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args([
            "dispatch",
            unit,
            "--task",
            "Wait for the test to report terminal completion.",
            shape,
        ])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &caller.pane_id)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run the real dispatch CLI")
}

fn run_work_done(deck: &TuiDeck, pane_id: &str, cwd: &Path, report: &str) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["work-done", "--done", "--task", report])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", pane_id)
        .env("HOME", deck.home_dir())
        .current_dir(cwd)
        .output()
        .expect("run the real work-done CLI")
}

fn assert_cli_succeeded(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} exited {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn pane_text(deck: &TuiDeck, pane: &PaneRef) -> String {
    common::strip_ansi(&common::pane_snapshot_on(
        deck.attach_socket_path(),
        &pane.agent_id,
    ))
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| "<daemon log not written>".to_string())
}

fn log_tail(path: &Path) -> String {
    let log = read_log(path);
    let mut lines: Vec<&str> = log.lines().rev().take(8).collect();
    lines.reverse();
    lines.join("\n")
}

fn line_has_word(line: &str, word: &str) -> bool {
    line.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|candidate| candidate == word)
}

fn submitted_ack_is_visible(deck: &TuiDeck, unit: &str) -> bool {
    let quoted_unit = format!("'{unit}'");
    deck.snapshot_grid().lines().any(|line| {
        line.contains("SUBMITTED:dispatch:")
            && line.contains("spawned isolated")
            && line.contains(&quoted_unit)
    })
}

fn rightmost_pane_cell(line: &str) -> Option<&str> {
    let (before_right_border, _) = line.rsplit_once('│')?;
    before_right_border.rsplit_once('│').map(|(_, cell)| cell)
}

fn submitted_completion_region(grid: &str) -> Option<String> {
    const OPENING: &str = "SUBMITTED:dispatch: a unit you dispatched has completed";
    const REPORT_CLOSE: &str = ":END-UNTRUSTED-WORKER-REPORT]";

    let pane_cells: Vec<&str> = grid.lines().filter_map(rightmost_pane_cell).collect();
    for (start, cell) in pane_cells.iter().enumerate() {
        let Some(opening) = cell.find(OPENING) else {
            continue;
        };

        let mut region = cell[opening..].to_string();
        if region.contains(REPORT_CLOSE) {
            return Some(region);
        }
        for continuation in &pane_cells[start + 1..] {
            if continuation.starts_with("SUBMITTED:")
                || continuation.starts_with("NOTICE:")
                || continuation.trim().is_empty()
            {
                break;
            }
            region.push_str(continuation);
            if region.contains(REPORT_CLOSE) {
                return Some(region);
            }
        }
    }
    None
}

fn submitted_return_is_visible(deck: &TuiDeck, unit: &str, report: &str) -> bool {
    let grid = deck.snapshot_grid();
    let Some(region) = submitted_completion_region(&grid) else {
        return false;
    };
    let key = common::search_key(&region);
    let unit_frame = common::search_key(&format!(
        "[UNTRUSTED-ROLE-LABEL: {unit} :END-UNTRUSTED-ROLE-LABEL]"
    ));
    let report_frame = common::search_key(&format!(
        "[UNTRUSTED-WORKER-REPORT: {report} :END-UNTRUSTED-WORKER-REPORT]"
    ));
    region.starts_with("SUBMITTED:dispatch:")
        && line_has_word(&region, "completed")
        && key.contains(&unit_frame)
        && key.contains(&report_frame)
}

fn wait_for_submitted_ack(deck: &TuiDeck, unit: &str, caller: &PaneRef, log: &Path) {
    assert!(
        common::wait_until(Duration::from_secs(60), || submitted_ack_is_visible(
            deck, unit
        )),
        "dispatch {unit:?} never reached the caller as a SUBMITTED acknowledgement; \
         the return-edge assertion would be meaningless without this control.\n\
         Caller PTY:\n{}\nDaemon log:\n{}\nFinal grid:\n{}",
        pane_text(deck, caller),
        log_tail(log),
        deck.snapshot_grid()
    );
}

fn wait_for_orchestrator(deck: &TuiDeck, worktree: &Path) -> PaneRef {
    let expected_dir = worktree
        .file_name()
        .expect("worktree has a basename")
        .to_string_lossy()
        .into_owned();
    let find = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|record| {
                let in_worktree = record.cwd.as_deref().is_some_and(|cwd| {
                    Path::new(cwd)
                        .file_name()
                        .is_some_and(|name| name == expected_dir.as_str())
                });
                in_worktree
                    && matches!(
                        record.tab_membership,
                        Some(TabMembership::Orchestration {
                            is_start_role: true,
                            ..
                        })
                    )
            })
            .and_then(|record| {
                Some(PaneRef {
                    pane_id: record.pane_id_env?,
                    agent_id: record.id,
                })
            })
    };
    assert!(
        common::wait_until(Duration::from_secs(30), || find().is_some()),
        "the dispatched orchestration never registered its start-role pane in {}.\n\
         Records: {:?}\nFinal grid:\n{}",
        worktree.display(),
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|record| (
                record.id.clone(),
                record.pane_id_env.clone(),
                record.cwd.clone(),
                record.tab_membership.clone()
            ))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    find().expect("the role-registration poll found the orchestrator")
}

fn wait_for_single_unit(deck: &TuiDeck, worktree: &Path, unit: &str) -> PaneRef {
    let expected_dir = worktree
        .file_name()
        .expect("worktree has a basename")
        .to_string_lossy()
        .into_owned();
    let expected_name = format!("dispatch-{unit}");
    let find = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|record| {
                record.display_name.as_deref() == Some(expected_name.as_str())
                    && record.cwd.as_deref().is_some_and(|cwd| {
                        Path::new(cwd)
                            .file_name()
                            .is_some_and(|name| name == expected_dir.as_str())
                    })
            })
            .and_then(|record| {
                Some(PaneRef {
                    pane_id: record.pane_id_env?,
                    agent_id: record.id,
                })
            })
    };
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            find().is_some_and(|pane| {
                common::pane_search_key_on(deck.attach_socket_path(), &pane.agent_id)
                    .contains(PROBE_READY)
            })
        }),
        "the dispatched single unit {expected_name:?} never registered and reached its \
         scripted ready marker.\nRecords: {:?}\nFinal grid:\n{}",
        common::agent_records_on(deck.attach_socket_path())
            .iter()
            .map(|record| (
                record.id.clone(),
                record.pane_id_env.clone(),
                record.cwd.clone(),
                record.display_name.clone()
            ))
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    find().expect("the readiness poll found the single unit")
}

fn assert_return_reached_caller(
    deck: &TuiDeck,
    caller: &PaneRef,
    unit: &str,
    report: &str,
    log: &Path,
    structural_reason: &str,
) {
    assert!(
        common::wait_until(RETURN_WAIT, || submitted_return_is_visible(
            deck, unit, report
        )),
        "the terminal completion for dispatched unit {unit:?} never appeared in the \
         CALLER'S rendered pane as a submitted turn within {}s. Expected one wrapped \
         message region beginning `SUBMITTED:dispatch:`, carrying {unit:?} in an \
         `UNTRUSTED-ROLE-LABEL` frame, the word `completed`, and report {report:?} \
         in an `UNTRUSTED-WORKER-REPORT` frame. {structural_reason}\nCaller PTY:\n{}\n\
         Daemon log:\n{}\nFinal grid:\n{}",
        RETURN_WAIT.as_secs(),
        pane_text(deck, caller),
        log_tail(log),
        deck.snapshot_grid()
    );
}

/// Scenario: Dispatch the fixture's token-free two-role orchestration from a
/// live caller pane, then have its orchestrator report terminal completion. The
/// unit name and distinctive report must return to that caller as a submitted turn.
#[spec("dispatch/return/001")]
#[test]
fn dispatch_return_001_orchestration_completion_reaches_the_caller() {
    const UNIT: &str = "orch-return-probe";
    const REPORT: &str = "orchestration-return-report-7f31";

    let scratch = common::race_safe_tempdir();
    let (config, log) = write_submit_probe(scratch.path());
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        .with_env("DOT_AGENT_DECK_LOG", log.to_string_lossy())
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());

    let caller = open_probe_caller(&deck);
    let worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(worktree.clone());
    let dispatched = run_dispatch(&deck, &caller, UNIT, "--orchestration=demo-orch");
    assert_cli_succeeded("dispatch --orchestration=demo-orch", &dispatched);
    wait_for_submitted_ack(&deck, UNIT, &caller, &log);

    let orchestrator = wait_for_orchestrator(&deck, &worktree);
    let completed = run_work_done(&deck, &orchestrator.pane_id, &worktree, REPORT);
    assert_cli_succeeded("dispatched orchestrator work-done --done", &completed);

    assert_return_reached_caller(
        &deck,
        &caller,
        UNIT,
        REPORT,
        &log,
        "Today's handler recognizes the role, logs `orchestration complete \
         (orchestrator --done)`, and returns after discarding the caller identity; \
         this is RED until the dispatch callback survives to that branch.",
    );
}

/// Scenario: Dispatch a token-free orchestration, detach its caller pane back
/// to the dashboard, and reattach that same live pane before terminal completion.
/// The completion report must still return there as a submitted turn.
#[spec("dispatch/return/002")]
#[test]
fn dispatch_return_002_callback_survives_caller_detach_and_reattach() {
    const UNIT: &str = "reattach-return-probe";
    const REPORT: &str = "reattach-return-report-82ac";

    let scratch = common::race_safe_tempdir();
    let (config, log) = write_submit_probe(scratch.path());
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        .with_env("DOT_AGENT_DECK_LOG", log.to_string_lossy())
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());

    let caller = open_probe_caller(&deck);
    let worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(worktree.clone());
    let dispatched = run_dispatch(&deck, &caller, UNIT, "--orchestration=demo-orch");
    assert_cli_succeeded("dispatch --orchestration=demo-orch", &dispatched);
    wait_for_submitted_ack(&deck, UNIT, &caller, &log);
    let orchestrator = wait_for_orchestrator(&deck, &worktree);

    // Exercise the real UI's pane detach/reattach transition between dispatch
    // and completion. In split layout the detached Dashboard still previews
    // the selected pane, so the mode footer -- COMMAND, then TYPING -- is the
    // observable state change; the acknowledgement identifies that preview as
    // the original caller throughout.
    deck.send_keys(b"\x04");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let grid = deck.snapshot_grid();
            grid.contains("COMMAND")
                && grid.contains("caller")
                && submitted_ack_is_visible(&deck, UNIT)
        }),
        "Ctrl+D did not detach the caller into Dashboard command mode while \
         retaining that caller's acknowledged pane as the selected preview.\n\
         Final grid:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"\x04");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let grid = deck.snapshot_grid();
            grid.contains("TYPING")
                && !grid.contains("COMMAND")
                && submitted_ack_is_visible(&deck, UNIT)
        }),
        "the second Ctrl+D did not reattach the SAME acknowledged caller pane in \
         typing mode.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    let completed = run_work_done(&deck, &orchestrator.pane_id, &worktree, REPORT);
    assert_cli_succeeded("reattached orchestration work-done --done", &completed);
    assert_return_reached_caller(
        &deck,
        &caller,
        UNIT,
        REPORT,
        &log,
        "The dispatched orchestrator's existing terminal branch still discards the \
         callback; detach/reattach has proved the original caller pane itself stayed \
         alive and was restored correctly.",
    );
}

/// Scenario: Dispatch a token-free single unit, first send `work-done` from the
/// unrelated caller as an unknown-pane control, then complete from the dispatched
/// unit. Only the unit's distinctive report may return to the caller as a submitted turn.
#[spec("dispatch/return/003")]
#[test]
fn dispatch_return_003_single_completion_routes_while_unknown_pane_stays_inert() {
    const UNIT: &str = "single-return-probe";
    const REPORT: &str = "single-return-report-4d92";
    const UNKNOWN_REPORT: &str = "unknown-pane-report-93e1-must-not-route";

    let scratch = common::race_safe_tempdir();
    let (config, log) = write_submit_probe(scratch.path());
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(200, 50)
        .with_env("PATH", path_with_binary_dir())
        .with_env("DOT_AGENT_DECK_CONFIG", config.to_string_lossy())
        .with_env("DOT_AGENT_DECK_LOG", log.to_string_lossy())
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");
    commit_fixture_repo(deck.workdir());

    let caller = open_probe_caller(&deck);
    let worktree = dispatch_worktree_of(&deck, UNIT);
    let _guard = SiblingWorktreeGuard(worktree.clone());
    let dispatched = run_dispatch(&deck, &caller, UNIT, "--single");
    assert_cli_succeeded("dispatch --single", &dispatched);
    wait_for_submitted_ack(&deck, UNIT, &caller, &log);
    let single = wait_for_single_unit(&deck, &worktree, UNIT);

    // Non-regression control: this registered caller is neither an orchestration
    // role nor a dispatched unit. Its report must take the existing warning path
    // and reach no pane. The later positive return is the drain barrier for the
    // absence check, so this cannot pass merely because the daemon was slow.
    let unknown = run_work_done(&deck, &caller.pane_id, deck.workdir(), UNKNOWN_REPORT);
    assert_cli_succeeded("unknown-pane work-done --done control", &unknown);
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            let current = read_log(&log);
            current.contains("work-done from unknown pane") && current.contains(&caller.pane_id)
        }),
        "the non-role, non-dispatched caller did not take the existing `work-done \
         from unknown pane` warning path.\nDaemon log:\n{}",
        log_tail(&log)
    );

    let completed = run_work_done(&deck, &single.pane_id, &worktree, REPORT);
    assert_cli_succeeded("dispatched single work-done --done", &completed);
    assert_return_reached_caller(
        &deck,
        &caller,
        UNIT,
        REPORT,
        &log,
        "Today's single pane has no `pane_role_map` identity, so the daemon logs \
         `work-done from unknown pane` for it too and returns before any completion \
         routing can run.",
    );

    let leaked_to: Vec<String> = common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .filter_map(|record| {
            let text = common::strip_ansi(&common::pane_snapshot_on(
                deck.attach_socket_path(),
                &record.id,
            ));
            text.contains(UNKNOWN_REPORT).then_some(record.id)
        })
        .collect();
    assert!(
        leaked_to.is_empty(),
        "the unknown-pane control report {UNKNOWN_REPORT:?} was delivered to pane(s) \
         {leaked_to:?}. The successful single-unit return above is the barrier proving \
         the daemon drained both work-done signals."
    );
}
