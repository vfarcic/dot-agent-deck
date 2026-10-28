#![cfg(all(feature = "e2e", target_os = "linux"))]

//! The editor helper's post-editor path and its reports, driven through the
//! `e2e` build's seams (`src/composer_probe/helper.rs`): injected start and
//! wait failures, a deadline that has already passed, hostile daemon peers,
//! the signal state the editor gets, literal tool values and amendment 6's
//! RED mutant. The test process owns the instance and the hook socket, which
//! `tests/support/editor_helper_faults.py` inherits as fd 3 and answers as
//! the daemon. No daemon, agent or user configuration.
mod common;

use dot_agent_deck::composer_probe::install::ProbeAgent;
use dot_agent_deck::composer_probe::instance::{DaemonId, Instance};
use spec::spec;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::process::Command;

fn run(case: &str) {
    run_with(case, |_| {});
}

/// [`run`], with the instance's daemon record changed first.
fn run_with(case: &str, record: impl FnOnce(&mut DaemonId)) {
    let dir = common::race_safe_tempdir();
    let mut daemon = DaemonId::this_process().unwrap();
    record(&mut daemon);
    let instance = Instance::create(
        &dir.path().join("editor"),
        daemon,
        &mut File::open(env!("CARGO_BIN_EXE_dot-agent-deck")).unwrap(),
    )
    .unwrap();
    let mut generation = instance
        .generation("3", Some(ProbeAgent::Codex), None)
        .unwrap();
    // No arm, with the segment and native numbers the helper reports.
    generation.arm().publisher().publish_native(0, 17, 23);
    // This process listens, so its pid is the peer the helper checks
    // against the instance's `daemon` record; the script accepts.
    let listener = UnixListener::bind(dir.path().join("hook.sock")).unwrap();
    let fd = listener.as_raw_fd();
    let mut command = Command::new("python3");
    command
        .arg("-c")
        .arg(include_str!("support/editor_helper_faults.py"))
        .arg(case)
        .arg(instance.helper())
        .arg(dir.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("OTEL_SDK_DISABLED", "true")
        .current_dir(dir.path());
    // SAFETY: `dup2` and `fcntl` are async-signal-safe. A new fd 3 has no
    // close-on-exec; when the listener already is fd 3, its flag is cleared.
    unsafe {
        command.pre_exec(move || {
            let rc = if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, 3)
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{case}: stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Scenario: The agent's own Ctrl+G, with no arm, reaches a daemon peer that answers, and starting the user's editor fails with EACCES through the seam. The helper reports `spawn_failed` with that errno on its connection, opens no editor, writes nothing to stdout or stderr, and exits 127 within a second of the failure.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_failed_start_reports_its_errno_and_writes_nothing() {
    run("spawn-failed-pipe");
}

/// Scenario: The same failed start with the helper's terminal a fixture pty whose output is stopped. Nothing reaches the master, the report carries the errno, and the helper exits 127 by its deadline.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_failed_start_needs_no_stopped_terminal() {
    run("spawn-failed-stopped");
}

/// Scenario: The same failed start on a fixture pty whose output queue is already full. The master holds only the fixture's own bytes, and the helper reports and exits 127 by its deadline.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_failed_start_needs_no_full_terminal() {
    run("spawn-failed-full");
}

/// Scenario: The same failed start with the helper in a background process group of a fixture terminal with TOSTOP set and SIGTTOU at its default. The helper is never stopped, writes nothing, and exits 127 by its deadline.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_failed_start_in_the_background_does_not_stop() {
    run("spawn-failed-background");
}

/// Scenario: The user's editor starts, and the helper's wait for it fails through the seam. The helper sends no report, exits 1 within a second of the failure with no output, and the editor goes on running and finishes after the helper is gone.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_unknown_wait_reports_nothing_and_the_editor_survives() {
    run("wait-unknown-pipe");
}

/// Scenario: The failed wait with the helper on a stopped fixture terminal: zero bytes at the master, exit 1 by the deadline, no report, and the editor finishes on its own.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_unknown_wait_needs_no_stopped_terminal() {
    run("wait-unknown-stopped");
}

/// Scenario: The failed wait on a fixture terminal whose output queue is full: no helper bytes, exit 1 by the deadline, no report, and the editor finishes.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_unknown_wait_needs_no_full_terminal() {
    run("wait-unknown-full");
}

/// Scenario: The failed wait with the helper in a background group of a TOSTOP fixture terminal: it is never stopped, writes nothing, exits 1 by the deadline, and the editor finishes.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_unknown_wait_in_the_background_does_not_stop() {
    run("wait-unknown-background");
}

/// Scenario: The seam pauses the helper right after its editor exits 37, at a monotonic `t0` it traces, and the test releases it only 300 ms past `t0 + 1 s`. The helper then skips its reap loop and its report, calls no poll and no send, the peer receives no report, and it exits 37 at once.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_passed_deadline_skips_the_report() {
    run("expired-deadline");
}

/// Scenario: A peer that answers the invocation and then never reads, with the helper's send buffer made small and its report padded to 4 MiB: part of the frame goes out, the rest waits on the deadline, no call starts after it, and the helper exits with the editor's 37 within a second of the editor's exit.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_unread_peer_costs_at_most_the_deadline() {
    run("unread-peer");
}

/// Scenario: A peer that reads exactly half of the padded report and stops: the helper sends what the peer takes, gives up at the deadline with the report unsent, and exits 37 within a second of the editor's exit.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_peer_that_reads_half_a_frame_costs_at_most_the_deadline() {
    run("half-frame");
}

/// Scenario: A peer that advertises the editor frontier reads the helper's clean report and never acknowledges it: the helper, single-threaded when it started the editor, waits for the acknowledgement until its deadline and no longer, then exits 37.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_absent_ack_is_waited_for_until_the_deadline() {
    run("absent-ack");
}

/// Scenario: The same peer sends half an acknowledgement line and stops, while the test interrupts the helper with a signal every 20 ms: interrupted polls and the partial read are retried against the same deadline, and the helper exits 37 by it.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_interruptions_and_partial_reads_are_charged_to_the_deadline() {
    run("eintr-partial-ack");
}

/// Scenario: A peer that advertises the editor frontier answers the clean report at once: the helper's request carried its decision, claim, generation, the segment and native numbers it read and the editor's argument, it left an `unarmed` marker with those numbers, the editor got the export path alone, and the helper exits 37 straight after the acknowledgement.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_clean_report_is_acknowledged() {
    run("clean-ack");
}

/// Scenario: The user's editor starts a background process in its own session and exits: the helper, its descendants' subreaper, finds the live child and reports `survivors`, without waiting for an acknowledgement.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_surviving_descendant_is_reported() {
    run("survivors");
}

/// Scenario: The agent starts the helper with SIGALRM, SIGINT and SIGCHLD ignored and SIGALRM and SIGUSR2 blocked. The editor starts with exactly that mask and those signals ignored, SIGQUIT at its default, and the helper still reaps it and returns its 37.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_the_editor_gets_the_inherited_signal_state() {
    run("inherited-signals");
}

/// Scenario: SIGINT and SIGQUIT sent to the helper while the editor runs, as a terminal sends them to the foreground group, leave it running, and it returns the editor's 37.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_terminal_signals_during_an_edit_leave_the_helper_running() {
    run("sigint-during-edit");
}

/// Scenario: A tool's invocation (no identity and no export shape) runs a saved editor value, naming the editor by its basename on a `PATH` where a directory and a non-executable file of that name come first, and holding `;`, `$(…)`, `|`, a redirection and backticks. The editor receives each as a literal argument before the file, and nothing is evaluated: no file is created by the command substitution or the redirection.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_tool_value_runs_literally() {
    run("literal-tool-value");
}

/// Scenario: Amendment 6's RED control, run against both paths on a stopped fixture terminal after a failed start. The A5 mutant, held after arming its no-op alarm until the handler has run, then writes and is still blocked 2 s past its deadline; the design writes nothing and exits 127 by its deadline.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_the_a5_alarm_mutant_hangs_where_the_design_exits() {
    run("mutant-a5");
    run("design-a5-row");
}

/// Scenario: With the arm present, the agent's own Ctrl+G reaches a peer that answers `uncertain`: the helper reported `armed` with the segment and native numbers and left an `armed` marker with them, prints exactly the notice, keeps it for 1.5 s, opens no editor, sends no report and exits 1 on Codex.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_uncertain_answer_shows_the_notice_for_its_wait() {
    run("armed-uncertain");
}

/// Scenario: With the arm present, a peer that answers `mine` with an empty read: no notice, no wait, no editor and no report, and the helper exits 0 on Codex at once.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_attested_empty_read_exits_quietly() {
    run("armed-mine-empty");
}

/// Scenario: The generation's arm page cut to exactly its three words with a count of 0 reads as absent: the user's editor runs and no notice is printed.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_page_of_exactly_three_words_reads() {
    run("arm-exact-words");
}

/// Scenario: A Codex generation whose user had `VISUAL` set and empty: the agent's own Ctrl+G with no arm opens no editor, as Codex refuses that value, reports `spawn_failed` with no errno, and exits 1.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_codex_refuses_an_empty_visual() {
    run("codex-empty-visual");
}

/// Scenario: The instance's daemon record names the listening process with another start time: the helper connects, sends that peer nothing, and still runs the user's editor, returning its 37.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_peer_with_another_start_is_sent_nothing() {
    run_with("foreign-start", |daemon| daemon.start += 1);
}

/// Scenario: The record names the listening process and its start, on another boot: the helper sends that peer nothing and runs the user's editor.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_peer_on_another_boot_is_sent_nothing() {
    run_with("foreign-boot", |daemon| {
        daemon.boot = "00000000-0000-0000-0000-000000000000".into();
    });
}

/// Scenario: A peer that hangs up right after answering the invocation: the helper's report fails to send, which it gives up on at once, and it still returns the editor's 37 by its deadline.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_peer_that_hangs_up_costs_nothing() {
    run("peer-gone");
}

/// Scenario: A peer that reads the capability question and hangs up without answering: the helper's exchange ends at once rather than at its 2 s bound, and the user's editor runs and returns 37.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_peer_that_hangs_up_unanswered_ends_the_exchange_at_once() {
    run("peer-eof");
}

/// Scenario: Without the seam, the user's editor value names an executable file that isn't a program, so starting it fails in the child with ENOEXEC: the helper reports `spawn_failed` with that errno, opens no editor and exits 127 by its deadline.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_a_real_failed_exec_reports_its_errno() {
    run("exec-fails");
}

/// Scenario: The user's editor leaves a child it never waited for, which has already exited: the helper reaps it as its subreaper, finds no other child, and reports a `clean` tree.
#[spec("inbox/probe/067")]
#[test]
fn probe_067_an_exited_descendant_is_reaped_and_the_tree_is_clean() {
    run("reaped-descendant");
}
