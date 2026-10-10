#![cfg(all(feature = "e2e", unix))]

//! Issue #1650: a `clear = false` delegate to a Codex worker that is still
//! starting. The worker is a narrow stand-in for codex-cli 0.160.0's start-up,
//! read back from the issue's own Codex logs: it loads for a while in cooked
//! mode, then takes the terminal into a provisional composer, and a prompt
//! typed and confirmed there is submitted at the hand-over as its last few
//! characters only. It reports every submission through Codex's own prompt
//! hook, as a trusted real Codex does.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TuiDeck, agent_records_on};
use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

/// What the delegate types: the pointer to the task file, then its delivery id.
const POINTER_HEAD: &str = "Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-";

/// argv: the deck binary, seconds of cooked-mode loading, seconds the
/// provisional composer lasts, and optionally `inline`: draw on the main
/// screen and never clear it, so the typed pointer's echo stays in view.
const BOOTING_CODEX_WORKER: &str = r#"import json
import os
import select
import subprocess
import sys
import termios
import time
import tty

deck, loading, startup = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
inline = sys.argv[4:] == ['inline']
clear = b'' if inline else b'\x1b[2J\x1b[H'
pid = os.getpid()


def log(name, text):
    with open(name, 'a', encoding='utf-8') as handle:
        handle.write(text + '\n')


log('worker-launches.log', str(pid))
# Codex loads its config before it draws anything, in cooked mode: about five
# seconds in the issue's logs, under load.
time.sleep(loading)
fd = sys.stdin.fileno()
out = sys.stdout.fileno()
# TCSANOW, so whatever was typed while it loaded is still there to read.
tty.setraw(fd, termios.TCSANOW)
handoff_at = time.monotonic() + startup
# Full screen on the alternate screen, as Codex draws.
os.write(out, (b'\r\n' if inline else b'\x1b[?1049h' + clear) + b'\xe2\x80\xba ')


def submit(text):
    log('worker-submits.log', text)
    turn = subprocess.run([deck, 'hook', '--agent', 'codex'],
        input=json.dumps({'hook_event_name': 'UserPromptSubmit',
            'session_id': f'booting-codex-{pid}', 'prompt': text}),
        text=True, capture_output=True, timeout=5)
    if turn.returncode:
        raise SystemExit(turn.stderr)
    # Redrawn like a full-screen TUI: the transcript, then an empty composer.
    os.write(out, (b'\r\n' if inline else clear) + b'\xe2\x80\xba ' + text.encode()
        + b'\r\n\r\n\xe2\x80\xba ')


line = bytearray()
starting = True
confirmed = False
while True:
    timeout = max(0.0, handoff_at - time.monotonic()) if starting else None
    readable, _, _ = select.select([fd], [], [], timeout)
    if starting and time.monotonic() >= handoff_at:
        starting = False
        if confirmed and line:
            # The hand-over as the issue's Codex logs show it: a draft typed
            # and confirmed while starting is submitted as its tail only.
            submit(line[-5:].decode('utf-8', 'replace'))
            line.clear()
        confirmed = False
    if not readable:
        continue
    chunk = os.read(fd, 4096)
    with open('worker-raw.log', 'ab') as raw:
        raw.write(chunk)
    for byte in chunk:
        if byte in (10, 13):
            if starting:
                confirmed = True
            elif line:
                submit(line.decode('utf-8', 'replace'))
                line.clear()
            continue
        line.append(byte)
        os.write(out, bytes([byte]))
"#;

fn launch_pids(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn submissions(work: &Path) -> Vec<String> {
    std::fs::read_to_string(work.join("worker-submits.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn is_whole_pointer(submission: &str) -> bool {
    submission.starts_with(POINTER_HEAD) && submission.ends_with(']')
}

/// Open a `clear = false` orchestration whose `coder` is the booting Codex
/// stand-in, and delegate to it the moment it has launched, while it is still
/// loading — as the orchestration's first delegate and `pane restart`'s next
/// one did in the issue.
fn delegate_into_booting_worker(timings: &[(&str, &str)]) -> (TuiDeck, PathBuf) {
    delegate_into_booting_worker_drawn(timings, "")
}

/// [`delegate_into_booting_worker`], with `draw` passed to the stand-in as its
/// last argument (`inline` or nothing).
fn delegate_into_booting_worker_drawn(timings: &[(&str, &str)], draw: &str) -> (TuiDeck, PathBuf) {
    let deck = timings
        .iter()
        .fold(
            TuiDeck::builder()
                .impersonating_pane_signals()
                .with_pty_size(120, 40)
                .with_env("DOT_AGENT_DECK_LOG", "booting-worker.log")
                .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0"),
            |builder, (key, value)| builder.with_env(*key, *value),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let work = deck.workdir().to_path_buf();
    std::fs::write(work.join("worker.py"), BOOTING_CODEX_WORKER).expect("write booting worker");
    let worker_command = format!(
        "python3 -u worker.py {} 3 1 {draw}",
        env!("CARGO_BIN_EXE_dot-agent-deck")
    );
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\n\
             name = \"booting-worker\"\n\
             [[orchestrations.roles]]\n\
             name = \"orchestrator\"\n\
             command = \"cat\"\n\
             start = true\n\
             [[orchestrations.roles]]\n\
             name = \"coder\"\n\
             command = \"{worker_command}\"\n\
             agent = \"codex\"\n\
             clear = false\n"
        ),
    )
    .expect("write orchestration config");

    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.wait_for_absence("Command:");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
    deck.wait_for_string("coder");

    let launches = work.join("worker-launches.log");
    assert!(
        common::wait_until(Duration::from_secs(10), || !launch_pids(&launches)
            .is_empty()),
        "worker did not launch; grid:\n{}",
        deck.snapshot_grid()
    );
    let orchestrator = agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|record| {
            matches!(
                &record.tab_membership,
                Some(TabMembership::Orchestration { role_name, is_start_role: true, .. })
                    if role_name == "orchestrator"
            )
        })
        .expect("orchestrator role has a daemon record");
    let orchestrator_pane = orchestrator.pane_id_env.expect("orchestrator pane id");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["delegate", "--to", "coder", "--task", "check the pointer"])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &orchestrator_pane)
        .output()
        .expect("run real delegate CLI");
    assert!(
        output.status.success(),
        "delegate CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        submissions(&work).is_empty(),
        "the worker submitted something before it finished loading: {:?}",
        submissions(&work)
    );
    (deck, work)
}

/// Scenario: Open an orchestration whose `clear = false` coder is a Codex
/// stand-in that is still loading, and delegate to it at once. The deck must
/// hold the task pointer until the worker is up, so the worker submits the
/// whole pointer once and never a cut-short piece of it.
#[spec("orchestration/delegate/052")]
#[test]
fn delegate_052_clear_false_pointer_waits_for_a_booting_codex_worker() {
    // The buffer the deck holds after the worker takes the terminal outlasts
    // the stand-in's one-second provisional composer, as the production 5 s
    // interface buffer outlasted the ~1 s the issue's Codex spent in it.
    // The re-send on, on a short schedule, so the exactly-once check below
    // sees a wrongly repeated pointer (Greptile, PR #1659): the harness turns
    // it off by default.
    let (deck, work) = delegate_into_booting_worker(&[
        ("DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS", "2000"),
        ("DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS", "1500,1500"),
    ]);
    assert!(
        common::wait_until(Duration::from_secs(20), || !submissions(&work).is_empty()),
        "the worker never submitted anything; raw={:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(work.join("worker-raw.log")).unwrap_or_default()),
        deck.snapshot_grid()
    );
    // Past the whole schedule (1.5 s, 1.5 s, then 1.5 s more before it is
    // exhausted), so a re-send the deck wrongly thought it owed has landed.
    assert!(
        !common::wait_until(Duration::from_secs(6), || submissions(&work).len() > 1),
        "the worker submitted again after the whole pointer: {:?}",
        submissions(&work)
    );
    let submitted = submissions(&work);
    assert_eq!(
        submitted.len(),
        1,
        "the worker should submit exactly one prompt: {submitted:?}"
    );
    assert!(
        is_whole_pointer(&submitted[0]),
        "the worker must receive the whole task pointer, not part of it: {submitted:?}"
    );
}

/// Scenario: Delegate at once to a `clear = false` Codex stand-in that is still
/// loading, with no readiness buffer, so the pointer lands in its provisional
/// composer and is submitted as only its last characters. The deck must not
/// take that turn as the task arriving: it sends the pointer again, and the
/// worker then submits the whole pointer.
#[spec("orchestration/delegate/053")]
#[test]
fn delegate_053_a_cut_short_pointer_is_sent_again() {
    cut_short_pointer_is_sent_again("");
}

/// Scenario: As the test above, but the stand-in draws on the main screen and
/// never clears it, so the echo of the first, cut-short typing stays on screen
/// above its input box. The deck must not read that echo as the task having
/// been submitted: it sends the pointer again, and the worker then submits the
/// whole pointer.
#[spec("orchestration/delegate/053")]
#[test]
fn delegate_053_a_cut_short_pointer_is_sent_again_over_its_own_echo() {
    cut_short_pointer_is_sent_again("inline");
}

fn cut_short_pointer_is_sent_again(draw: &str) {
    let (deck, work) = delegate_into_booting_worker_drawn(
        &[
            ("DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS", "0"),
            ("DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS", "1500,3000"),
        ],
        draw,
    );
    assert!(
        common::wait_until(Duration::from_secs(25), || submissions(&work)
            .iter()
            .any(|submission| is_whole_pointer(submission))),
        "the worker never received the whole task pointer; submissions={:?}; daemon log:\n{}\ngrid:\n{}",
        submissions(&work),
        std::fs::read_to_string(work.join("booting-worker.log")).unwrap_or_default(),
        deck.snapshot_grid()
    );
    let submitted = submissions(&work);
    assert!(
        submitted[0].len() < POINTER_HEAD.len() && !is_whole_pointer(&submitted[0]),
        "the stand-in should first submit a cut-short piece, or this run did not reproduce \
         the issue: {submitted:?}"
    );
    // Long enough for one more re-send to land if the deck still owed one.
    let whole = |submitted: &[String]| {
        submitted
            .iter()
            .filter(|submission| is_whole_pointer(submission))
            .count()
    };
    assert!(
        !common::wait_until(Duration::from_secs(4), || whole(&submissions(&work)) > 1),
        "the whole pointer reached the worker more than once: {:?}",
        submissions(&work)
    );
    let submitted = submissions(&work);
    assert_eq!(
        submitted
            .iter()
            .filter(|submission| is_whole_pointer(submission))
            .count(),
        1,
        "the whole pointer must reach the worker exactly once: {submitted:?}"
    );
}
