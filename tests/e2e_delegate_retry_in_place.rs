#![cfg(all(feature = "e2e", unix))]

//! PTY-attached reproduction for a delegate pointer lost during a silent
//! worker's boot. The worker is a narrow OpenCode stand-in: it deliberately
//! consumes early PTY bytes without acting on them, then accepts input in the
//! same process.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{TuiDeck, agent_records_on};
use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

const POINTER: &str = "Read .dot-agent-deck/worker-task-coder.md";
const PROOF: &str = "DELEGATE_RETRY_SAME_PROCESS_1383";
const WORKER: &str = r#"import os
import select
import sys
import time
import tty

pid = os.getpid()
with open('worker-launches.log', 'a', encoding='ascii') as log:
    log.write(f'{pid}\n')

fd = sys.stdin.fileno()
tty.setraw(fd)
deadline = time.monotonic() + 3.5
while time.monotonic() < deadline:
    readable, _, _ = select.select([fd], [], [], max(0, deadline - time.monotonic()))
    if readable:
        discarded = os.read(fd, 4096)
        if discarded:
            with open('worker-discarded.log', 'ab') as log:
                log.write(discarded)

os.write(sys.stdout.fileno(), b'WORKER_READY_1383\r\n')
line = bytearray()
while True:
    for byte in os.read(fd, 4096):
        if byte in (10, 13):
            if b'worker-task-coder.md' in line:
                with open('worker-accepted-pid.log', 'w', encoding='ascii') as log:
                    log.write(str(pid))
                os.write(sys.stdout.fileno(), b'DELEGATE_RETRY_SAME_PROCESS_1383\r\n')
            line.clear()
        else:
            line.append(byte)
"#;

fn launch_pids(path: &std::path::Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| line.parse().expect("worker launch log contains a PID"))
        .collect()
}

/// Scenario: Open a real orchestration tab and delegate to a silent OpenCode
/// stand-in whose replacement process consumes and discards its first PTY input.
/// The task pointer must later reach that same process and print its proof in
/// the attached worker pane, without another respawn.
#[spec("orchestration/delegate/042")]
#[test]
fn delegate_042_retries_lost_pointer_in_the_same_worker_process() {
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(120, 40)
        .with_env("DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS", "200")
        .with_env(
            "DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS",
            "1500,3000,6000",
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");

    let work = deck.workdir();
    std::fs::write(work.join("worker.py"), WORKER).expect("write deaf worker stand-in");
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        "[[orchestrations]]\n\
         name = \"retry-in-place\"\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"cat\"\n\
         start = true\n\
         [[orchestrations.roles]]\n\
         name = \"coder\"\n\
         command = \"python3 -u worker.py\"\n\
         agent = \"opencode\"\n\
         clear = true\n",
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
        "precondition: the initial worker did not launch; grid:\n{}",
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
    let orchestrator_pane = orchestrator
        .pane_id_env
        .expect("orchestrator has a pane id");
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

    // `clear = true` replaces the initially spawned worker. Its second PID is
    // the occupant that receives the first write and must survive every retry.
    assert!(
        common::wait_until(Duration::from_secs(10), || launch_pids(&launches).len()
            >= 2),
        "precondition: delegate did not spawn its replacement; launches={:?}",
        launch_pids(&launches)
    );
    let delivered_pid = launch_pids(&launches)[1];
    let discarded = work.join("worker-discarded.log");
    assert!(
        common::wait_until(Duration::from_secs(12), || std::fs::read(&discarded)
            .is_ok_and(|bytes| bytes
                .windows(POINTER.len())
                .any(|part| part == POINTER.as_bytes()))),
        "precondition: the first pointer never entered the worker's deaf period; discarded={:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(&discarded).unwrap_or_default()).into_owned(),
        deck.snapshot_grid()
    );

    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent Ctrl+N]");
    deck.send_bytes(b"2");
    let proof_visible = deck.wait_for_grid_string_within(PROOF, Duration::from_secs(45));
    let accepted_pid = std::fs::read_to_string(work.join("worker-accepted-pid.log"))
        .ok()
        .and_then(|text| text.parse::<u32>().ok());
    let final_launches = launch_pids(&launches);
    assert!(
        proof_visible,
        "the worker discarded the initial task pointer and no in-place retry reached its ready input; discarded={:?}; accepted_pid={accepted_pid:?}; launches={final_launches:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(&discarded).unwrap_or_default()).into_owned(),
        deck.snapshot_grid()
    );
    assert_eq!(
        accepted_pid,
        Some(delivered_pid),
        "the process that accepted the retried pointer must be the same replacement that discarded the first write"
    );
    assert_eq!(
        final_launches,
        vec![final_launches[0], delivered_pid],
        "delivery recovery must not respawn the worker"
    );
    assert!(common::process_running(delivered_pid as i32));
}

/// Issue #1381's worker: an OpenCode stand-in whose boot outlasts the
/// production no-signal hold. It is deaf for at least `argv[2]` seconds from
/// its own launch — the ~12 s a loaded box needs where the hold ships 8 s
/// (`NO_SIGNAL_READINESS_BUFFER`'s own doc comment) — and, so that a slow
/// runner cannot hand it the first pointer after it is already listening, until
/// 4 s after its first input arrives. What arrives meanwhile is consumed without
/// being acted on, as a TUI's terminal-mode switch does. It
/// then echoes typed bytes like OpenCode's composer, and on a submitted pointer
/// sends what a real OpenCode sends once a prompt creates its session:
/// `session.created` then `session.prompt`, through the real hook CLI.
const LOADED_BOOT_WORKER: &str = r#"import json
import os
import select
import subprocess
import sys
import time
import tty

pid = os.getpid()
started = time.monotonic()
with open('worker-launches.log', 'a', encoding='ascii') as log:
    log.write(f'{pid}\n')

fd = sys.stdin.fileno()
tty.setraw(fd)
deadline = started + float(sys.argv[2])
first_input = None
while first_input is None or time.monotonic() < deadline:
    readable, _, _ = select.select([fd], [], [], max(0, deadline - time.monotonic()))
    if readable:
        discarded = os.read(fd, 4096)
        if discarded:
            if first_input is None:
                first_input = time.monotonic()
                deadline = max(deadline, first_input + 4)
            # The wall-clock time first, so a reader that sees the bytes always
            # finds when they arrived.
            with open('worker-discarded-at.log', 'a', encoding='ascii') as log:
                log.write(f'{time.time():.3f}\n')
            with open('worker-discarded.log', 'ab') as log:
                log.write(discarded)

os.write(sys.stdout.fileno(), b'Ask anything\r\n')
line = bytearray()
accepted = 0
while True:
    chunk = os.read(fd, 4096)
    for byte in chunk:
        if accepted:
            with open('worker-after-accept.log', 'ab') as log:
                log.write(bytes([byte]))
            continue
        if byte not in (10, 13):
            line.append(byte)
            os.write(sys.stdout.fileno(), bytes([byte]))
            continue
        if b'worker-task-coder.md' in line:
            accepted += 1
            with open('worker-accepted-pid.log', 'a', encoding='ascii') as log:
                log.write(f'{pid}\n')
            for event in ({'event': 'session.created'},
                          {'event': 'session.prompt', 'prompt': line.decode('utf-8', 'replace')}):
                event['session_id'] = f'loaded-boot-{pid}'
                hook = subprocess.run([sys.argv[1], 'hook', '--agent', 'opencode'],
                    input=json.dumps(event), text=True, capture_output=True, timeout=5)
                if hook.returncode:
                    raise SystemExit(hook.stderr)
            os.write(sys.stdout.fileno(), b'\r\nDELEGATE_RETRY_LOADED_BOOT_1381\r\n')
        line.clear()
"#;

/// Scenario: Delegate at the production timings — the 8 s no-signal hold, the
/// default retry schedule and silence window — to an OpenCode stand-in whose
/// boot takes 12 s and swallows the pointer typed during it (issue #1381). The
/// same process must later accept the task exactly once and show it in its
/// attached pane, and the orchestrator must not be told the worker went quiet.
#[spec("orchestration/delegate/042")]
#[test]
fn delegate_042_default_timings_recover_pointer_lost_to_a_loaded_opencode_boot() {
    let schedule = dot_agent_deck::delegate_retry::DEFAULT_RETRY_SCHEDULE_MS
        .map(|ms| ms.to_string())
        .join(",");
    let command = format!(
        "python3 -u worker.py {} 12",
        env!("CARGO_BIN_EXE_dot-agent-deck")
    );
    // The harness pins all three to off; an empty buffer and silence window
    // are not a number, so the daemon falls back to its production defaults
    // for both. An empty schedule means "disabled", so the default is spelled
    // out from the shipped constant; that an unset variable resolves to it is
    // pinned at L1 by `retry_schedule_unset_is_the_default`.
    let (deck, work, delivered_pid) = launch_retry_fixture_with_timings(
        LOADED_BOOT_WORKER,
        &command,
        "opencode",
        &[
            ("DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS", ""),
            ("DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS", &schedule),
            ("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", ""),
            // The hold's own debug line names the interval it resolved.
            ("RUST_LOG", "dot_agent_deck::state=debug"),
        ],
    );

    let discarded = work.join("worker-discarded.log");
    assert!(
        common::wait_until(Duration::from_secs(20), || std::fs::read(&discarded)
            .is_ok_and(|bytes| pointer_count(&bytes) >= 1)),
        "precondition: the first pointer never landed inside the 12 s boot; discarded={:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(&discarded).unwrap_or_default()).into_owned(),
        deck.snapshot_grid()
    );
    // The production hold was in force, by the daemon's own account: the
    // declared-no-signal path logs the interval it resolved before it waits.
    let daemon_log = std::fs::read_to_string(work.join("retry-loop.log")).unwrap_or_default();
    assert!(
        daemon_log.lines().any(|line| line
            .contains("holding the task prompt for the no-signal readiness buffer")
            && line.contains("buffer_ms=8000")),
        "precondition: the daemon did not hold the pointer for the shipped 8000 ms no-signal buffer; daemon log:\n{daemon_log}"
    );
    // And the bytes really arrived that late. The baseline is taken just
    // before the delegate CLI starts, so this is a sanity bound, not a
    // measurement of the hold: 7.5 s leaves room for clock granularity.
    let delegated_at: f64 = std::fs::read_to_string(work.join("delegate-sent-at.log"))
        .expect("fixture logged when it sent the delegate")
        .trim()
        .parse()
        .expect("numeric delegate time");
    let lost_at: f64 = std::fs::read_to_string(work.join("worker-discarded-at.log"))
        .expect("worker logged when it discarded input")
        .lines()
        .next()
        .and_then(|line| line.parse().ok())
        .expect("numeric discard time");
    let held = lost_at - delegated_at;
    assert!(
        held >= 7.5,
        "precondition: the pointer arrived {held:.3}s after the delegate was sent, so the 8 s no-signal hold was not in force"
    );

    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent Ctrl+N]");
    deck.send_bytes(b"2");
    let proof_visible = deck
        .wait_for_grid_string_within("DELEGATE_RETRY_LOADED_BOOT_1381", Duration::from_secs(75));
    assert!(
        proof_visible,
        "the worker lost the pointer typed during its boot and it never reached the booted worker; discarded={:?}; accepted={:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(&discarded).unwrap_or_default()).into_owned(),
        std::fs::read_to_string(work.join("worker-accepted-pid.log")).unwrap_or_default(),
        deck.snapshot_grid()
    );
    // The turn it reported stops the retry; nothing may reach it after that.
    wait_for_retry_loop_end(&work);
    assert_eq!(
        std::fs::read_to_string(work.join("worker-accepted-pid.log"))
            .expect("accepted PID log")
            .lines()
            .collect::<Vec<_>>(),
        vec![delivered_pid.to_string()],
        "the booted worker must accept the task exactly once, in the process that lost the first pointer"
    );
    let after_accept = std::fs::read(work.join("worker-after-accept.log")).unwrap_or_default();
    assert!(
        after_accept.is_empty(),
        "input reached the worker after it accepted the task: {:?}",
        String::from_utf8_lossy(&after_accept)
    );
    assert!(
        !orchestrator_text(&deck)
            .contains(&common::squeeze_wrapped_text("delegated worker went quiet")),
        "a recovered delegation was reported to the orchestrator as silent; grid:\n{}",
        deck.snapshot_grid()
    );
    assert_eq!(launch_pids(&work.join("worker-launches.log")).len(), 2);
    assert!(common::process_running(delivered_pid as i32));
}

const NEVER_READS_WORKER: &str = r#"import os
import time

with open('worker-launches.log', 'a', encoding='ascii') as log:
    log.write(f'{os.getpid()}\n')
while True:
    time.sleep(1)
"#;

const COMPOSER_WORKER: &str = r#"import os
import sys
import time
import tty

pid = os.getpid()
with open('worker-launches.log', 'a', encoding='ascii') as log:
    log.write(f'{pid}\n')
fd = sys.stdin.fileno()
tty.setraw(fd)
line = bytearray()
first_submit = None
while True:
    chunk = os.read(fd, 4096)
    with open('worker-raw.log', 'ab') as log:
        log.write(chunk)
    for byte in chunk:
        if byte in (10, 13):
            if b'worker-task-coder.md' in line:
                if first_submit is None:
                    first_submit = time.monotonic()
                elif time.monotonic() - first_submit >= 1.0:
                    with open('worker-accepted-pid.log', 'w', encoding='ascii') as log:
                        log.write(str(pid))
                    os.write(sys.stdout.fileno(), b'COMPOSER_SUBMITTED_1383\r\n')
                    line.clear()
            continue
        line.append(byte)
        os.write(sys.stdout.fileno(), bytes([byte]))
"#;

const READY_COMPOSER_WORKER: &str = r#"import json
import os
import subprocess
import sys
import time
import tty

pid = os.getpid()
with open('worker-launches.log', 'a', encoding='ascii') as log:
    log.write(f'{pid}\n')
fd = sys.stdin.fileno()
tty.setraw(fd)
hook_agent = 'claude-code' if sys.argv[2] == 'claude' else 'codex'
hook = subprocess.run([sys.argv[1], 'hook', '--agent', hook_agent],
    input='{"hook_event_name":"SessionStart","session_id":"ready-composer-%s"}' % pid,
    text=True, capture_output=True, timeout=5)
with open('worker-ready.log', 'a', encoding='ascii') as log:
    log.write(f'{pid} {hook.returncode}\n')
if hook.returncode:
    raise SystemExit(hook.stderr)
os.write(sys.stdout.fileno(), b'\xe2\x9d\xaf ' if sys.argv[2] == 'claude' else b'\xe2\x80\xba ')
line = bytearray()
submits = 0
accepted_at = None
before_accept = []
while True:
    chunk = os.read(fd, 4096)
    with open('worker-raw.log', 'ab') as log:
        log.write(chunk)
    for byte in chunk:
        received_at = time.monotonic_ns()
        if accepted_at is None:
            before_accept.append((received_at, byte))
        else:
            with open('worker-input-timeline.log', 'a', encoding='ascii') as log:
                log.write(f'{(received_at - accepted_at) / 1_000_000:.3f} {byte:02x}\n')
        if byte in (10, 13):
            submits += 1
            with open('worker-submits.log', 'a', encoding='ascii') as log:
                log.write(f'{pid} {submits} {line.decode("utf-8", "replace")}\n')
            if submits == 1:
                continue
            if b'worker-task-coder.md' in line:
                accepted_at = time.monotonic_ns()
                with open('worker-input-timeline.log', 'w', encoding='ascii') as log:
                    for timestamp, received_byte in before_accept:
                        log.write(f'{(timestamp - accepted_at) / 1_000_000:.3f} {received_byte:02x}\n')
                    log.write('ACCEPT 0\n')
                before_accept.clear()
                if sys.argv[2] == 'claude':
                    turn = subprocess.run([sys.argv[1], 'hook', '--agent', hook_agent],
                        input=json.dumps({'hook_event_name': 'UserPromptSubmit',
                            'session_id': f'ready-composer-{pid}',
                            'prompt': line.decode('utf-8', 'replace')}),
                        text=True, capture_output=True, timeout=5)
                    if turn.returncode:
                        raise SystemExit(turn.stderr)
                with open('worker-accepted-pid.log', 'w', encoding='ascii') as log:
                    log.write(str(pid))
                os.write(sys.stdout.fileno(), b'\r\nREADY_COMPOSER_SUBMITTED_1383\r\n')
                line.clear()
            continue
        line.append(byte)
        os.write(sys.stdout.fileno(), bytes([byte]))
"#;

const ACK_WORKER: &str = r#"import os
from pathlib import Path
import re
import subprocess
import sys
import tty

pid = os.getpid()
with open('worker-launches.log', 'a', encoding='ascii') as log:
    log.write(f'{pid}\n')
fd = sys.stdin.fileno()
tty.setraw(fd)
line = bytearray()
while True:
    chunk = os.read(fd, 4096)
    with open('worker-raw.log', 'ab') as log:
        log.write(chunk)
    for byte in chunk:
        if byte not in (10, 13):
            line.append(byte)
            continue
        match = re.search(rb'\[delivery (d-[0-9a-f]{8})\]', line)
        if match:
            delivery_id = match.group(1).decode('ascii')
            task = Path('.dot-agent-deck/worker-task-coder.md').read_text()
            header_present = f'dot-agent-deck ack {delivery_id}' in task
            if header_present:
                first = subprocess.run([sys.argv[1], 'ack', delivery_id],
                                       capture_output=True, text=True, timeout=5)
                second = subprocess.run([sys.argv[1], 'ack', delivery_id],
                                        capture_output=True, text=True, timeout=5)
                codes = (first.returncode, second.returncode)
            else:
                codes = (-1, -1)
            Path('worker-ack.log').write_text(
                f'{pid} {delivery_id} {header_present} {codes[0]} {codes[1]}')
            os.write(sys.stdout.fileno(), b'WORKER_ACK_ATTEMPTED_1383\r\n')
        line.clear()
"#;

fn launch_retry_fixture(
    worker: &str,
    worker_command: &str,
    schedule: &str,
    silence_window: &str,
) -> (TuiDeck, PathBuf, u32) {
    launch_retry_fixture_for_agent(worker, worker_command, "opencode", schedule, silence_window)
}

fn launch_retry_fixture_for_agent(
    worker: &str,
    worker_command: &str,
    agent: &str,
    schedule: &str,
    silence_window: &str,
) -> (TuiDeck, PathBuf, u32) {
    launch_retry_fixture_with_timings(
        worker,
        worker_command,
        agent,
        &[
            ("DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS", "200"),
            ("DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS", schedule),
            ("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", silence_window),
        ],
    )
}

fn launch_retry_fixture_with_timings(
    worker: &str,
    worker_command: &str,
    agent: &str,
    timings: &[(&str, &str)],
) -> (TuiDeck, PathBuf, u32) {
    let deck = timings
        .iter()
        .fold(
            TuiDeck::builder()
                .impersonating_pane_signals()
                .with_pty_size(120, 40)
                .with_env("DOT_AGENT_DECK_LOG", "retry-loop.log"),
            |builder, (key, value)| builder.with_env(*key, *value),
        )
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let work = deck.workdir().to_path_buf();
    std::fs::write(work.join("worker.py"), worker).expect("write synthetic worker");
    std::fs::write(
        work.join(".dot-agent-deck.toml"),
        format!(
            "[[orchestrations]]\n\
             name = \"retry-in-place\"\n\
             [[orchestrations.roles]]\n\
             name = \"orchestrator\"\n\
             command = \"cat\"\n\
             start = true\n\
             [[orchestrations.roles]]\n\
             name = \"coder\"\n\
             command = \"{worker_command}\"\n\
             agent = \"{agent}\"\n\
             clear = true\n"
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
        "initial worker did not launch; grid:\n{}",
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
    let sent_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_secs_f64();
    std::fs::write(work.join("delegate-sent-at.log"), format!("{sent_at:.3}"))
        .expect("record when the delegate was sent");
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
        common::wait_until(Duration::from_secs(10), || launch_pids(&launches).len()
            >= 2),
        "delegate did not spawn replacement worker; launches={:?}",
        launch_pids(&launches)
    );
    let delivered_pid = launch_pids(&launches)[1];
    (deck, work, delivered_pid)
}

fn pointer_count(raw: &[u8]) -> usize {
    raw.windows(POINTER.len())
        .filter(|part| *part == POINTER.as_bytes())
        .count()
}

fn orchestrator_text(deck: &TuiDeck) -> String {
    common::orchestration_pane_column(&deck.snapshot_grid())
        .map(|pane| common::squeeze_wrapped_text(&pane))
        .unwrap_or_default()
}

fn wait_for_retry_loop_end(work: &std::path::Path) {
    let log_path = work.join("retry-loop.log");
    assert!(
        common::wait_until(Duration::from_secs(30), || {
            std::fs::read_to_string(&log_path).is_ok_and(|log| {
                log.lines().any(|line| {
                    line.contains(
                        "delegate retry: schedule exhausted; the silent-worker report covers it",
                    ) || line.contains("delegate retry: every scheduled re-delivery went out")
                        || line.contains("delegate retry: stopped")
                })
            })
        }),
        "retry loop did not log a terminal outcome; daemon log:\n{}",
        std::fs::read_to_string(&log_path).unwrap_or_default()
    );
    // Let the daemon log flush and any final PTY bytes reach the worker log.
    let settle = std::time::Instant::now();
    assert!(common::wait_until(Duration::from_secs(1), || settle
        .elapsed()
        >= Duration::from_millis(200)));
}

/// Scenario: Delegate through the attached TUI to a known-agent stand-in that
/// stays alive but never reads its PTY or emits hooks. After three retries, the
/// orchestrator must visibly receive the not-delivered silence report and the
/// same replacement worker process must still be running.
#[spec("orchestration/delegate/043")]
#[test]
fn delegate_043_exhaustion_reports_three_retries_without_respawn() {
    let (deck, work, delivered_pid) = launch_retry_fixture(
        NEVER_READS_WORKER,
        "python3 -u worker.py",
        "500,1000,1500",
        "300",
    );
    let notice =
        common::squeeze_wrapped_text("delegated worker went quiet (dot-agent-deck daemon report)");
    assert!(
        common::wait_until(Duration::from_secs(12), || orchestrator_text(&deck)
            .contains(&notice)),
        "not-delivered silence notice never reached orchestrator; grid:\n{}",
        deck.snapshot_grid()
    );
    let text = orchestrator_text(&deck);
    assert!(
        text.contains(&common::squeeze_wrapped_text(
            "tried 3 more times to get the task into the same process"
        )),
        "silence notice omitted the three in-place retries; pane={text:?}; grid:\n{}",
        deck.snapshot_grid()
    );
    assert_eq!(launch_pids(&work.join("worker-launches.log")).len(), 2);
    assert!(common::process_running(delivered_pid as i32));
}

/// Scenario: Delegate to a hookless worker that renders typed pointer bytes in
/// its composer but ignores Enter for one second. Retries must submit that
/// visible pointer without typing a second copy, and the same process must
/// eventually accept the task.
#[spec("orchestration/delegate/044")]
#[test]
fn delegate_044_visible_composer_retries_submit_only() {
    let (deck, work, delivered_pid) = launch_retry_fixture(
        COMPOSER_WORKER,
        "python3 -u worker.py",
        "500,1000,1500",
        "0",
    );
    let raw_path = work.join("worker-raw.log");
    assert!(
        common::wait_until(Duration::from_secs(8), || std::fs::read(&raw_path)
            .is_ok_and(|raw| pointer_count(&raw) >= 1)),
        "the first pointer did not reach the worker; grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        common::wait_until(Duration::from_secs(12), || work
            .join("worker-accepted-pid.log")
            .exists()),
        "retry never submitted the visible pointer; raw={:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(&raw_path).unwrap_or_default()),
        deck.snapshot_grid()
    );
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent Ctrl+N]");
    deck.send_bytes(b"2");
    assert!(
        deck.wait_for_grid_string_within("COMPOSER_SUBMITTED_1383", Duration::from_secs(5)),
        "accepted task did not render in attached worker pane; grid:\n{}",
        deck.snapshot_grid()
    );
    wait_for_retry_loop_end(&work);
    let raw = std::fs::read(&raw_path).expect("worker raw-byte log");
    assert_eq!(
        pointer_count(&raw),
        1,
        "pointer was typed twice; raw={raw:?}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("worker-accepted-pid.log"))
            .expect("accepted PID")
            .parse::<u32>()
            .expect("numeric accepted PID"),
        delivered_pid
    );
    assert_eq!(launch_pids(&work.join("worker-launches.log")).len(), 2);
}

fn ready_composer_lost_submit(agent: &str) {
    let command = format!(
        "python3 -u worker.py {} {agent}",
        env!("CARGO_BIN_EXE_dot-agent-deck")
    );
    let (deck, work, delivered_pid) = launch_retry_fixture_for_agent(
        READY_COMPOSER_WORKER,
        &command,
        agent,
        "500,1000,1500",
        "0",
    );
    let ready_path = work.join("worker-ready.log");
    assert!(
        common::wait_until(Duration::from_secs(8), || {
            std::fs::read_to_string(&ready_path).is_ok_and(|ready| {
                ready
                    .lines()
                    .any(|line| line == format!("{delivered_pid} 0"))
            })
        }),
        "replacement worker did not announce readiness: {:?}",
        std::fs::read_to_string(&ready_path).unwrap_or_default()
    );
    let ready = std::fs::read_to_string(&ready_path).expect("worker readiness log");
    assert!(
        ready
            .lines()
            .any(|line| line == format!("{delivered_pid} 0")),
        "replacement worker did not announce readiness: {ready:?}"
    );
    let raw_path = work.join("worker-raw.log");
    assert!(
        common::wait_until(Duration::from_secs(8), || std::fs::read(&raw_path)
            .is_ok_and(|raw| pointer_count(&raw) >= 1)),
        "pointer never reached the ready worker; grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        common::wait_until(Duration::from_secs(12), || work
            .join("worker-accepted-pid.log")
            .exists()),
        "pointer stayed visibly unsent after the first CR; raw={:?}; submits={:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(&raw_path).unwrap_or_default()),
        std::fs::read_to_string(work.join("worker-submits.log")).unwrap_or_default(),
        deck.snapshot_grid()
    );
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[New Agent Ctrl+N]");
    deck.send_bytes(b"2");
    assert!(
        deck.wait_for_grid_string_within("READY_COMPOSER_SUBMITTED_1383", Duration::from_secs(5)),
        "submitted task did not render in the worker pane; grid:\n{}",
        deck.snapshot_grid()
    );
    wait_for_retry_loop_end(&work);
    let raw = std::fs::read(&raw_path).expect("worker raw-byte log");
    assert_eq!(pointer_count(&raw), 1, "pointer was typed twice: {raw:?}");
    let timeline = std::fs::read_to_string(work.join("worker-input-timeline.log"))
        .expect("worker input timeline after acceptance");
    let (_, after_accept) = timeline
        .split_once("ACCEPT 0\n")
        .expect("worker input timeline contains acceptance marker");
    assert!(
        after_accept.is_empty(),
        "worker received input after accepting the task; relative-ms and hex bytes:\n{after_accept}"
    );
    let submit_bytes = raw
        .iter()
        .filter(|&&byte| byte == b'\r' || byte == b'\n')
        .count();
    assert_eq!(
        submit_bytes, 2,
        "worker should receive one ignored Enter and one accepting Enter; raw={raw:?}; timeline:\n{timeline}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("worker-accepted-pid.log"))
            .expect("accepted PID")
            .parse::<u32>()
            .expect("numeric accepted PID"),
        delivered_pid
    );
    assert_eq!(launch_pids(&work.join("worker-launches.log")).len(), 2);
}

/// Scenario: A Claude-declared stand-in announces SessionStart, shows the task
/// pointer in its ❯ composer, and ignores the first Enter. A later Enter makes
/// it emit UserPromptSubmit and show completion without a second pointer copy
/// or any input after acceptance.
#[spec("orchestration/delegate/047")]
#[test]
fn delegate_047_ready_claude_composer_recovers_lost_submit() {
    ready_composer_lost_submit("claude");
}

/// Scenario: A Codex-declared stand-in announces readiness and shows the task
/// pointer in its › composer while dropping the first Enter. A submit-only
/// retry must complete the task in that process without typing again or
/// sending any input after acceptance.
#[spec("orchestration/delegate/048")]
#[test]
fn delegate_048_ready_codex_composer_recovers_lost_submit() {
    ready_composer_lost_submit("codex");
}

/// Scenario: A hookless worker reads the delivery id and acknowledgement
/// instruction from its real task file, then calls the real ack CLI twice.
/// Both calls must succeed, the pointer must be delivered once, and no silent
/// worker notice may reach the orchestrator after the retry window.
#[spec("orchestration/delegate/045")]
#[test]
fn delegate_045_ack_stops_retries_and_silence_notice() {
    let worker_command = format!(
        "python3 -u worker.py {}",
        env!("CARGO_BIN_EXE_dot-agent-deck")
    );
    let (deck, work, delivered_pid) =
        launch_retry_fixture(ACK_WORKER, &worker_command, "500,1000,1500", "300");
    let ack_path = work.join("worker-ack.log");
    assert!(
        common::wait_until(Duration::from_secs(10), || ack_path.exists()),
        "worker did not find the delivery id and task-file header; raw={:?}; grid:\n{}",
        String::from_utf8_lossy(&std::fs::read(work.join("worker-raw.log")).unwrap_or_default()),
        deck.snapshot_grid()
    );
    let ack = std::fs::read_to_string(&ack_path).expect("worker ack log");
    let fields: Vec<_> = ack.split_whitespace().collect();
    assert_eq!(fields.len(), 5, "ack log has wrong shape: {ack:?}");
    assert_eq!(fields[0], delivered_pid.to_string());
    assert_eq!(fields[2], "True", "task-file ack header missing: {ack:?}");
    assert_eq!(fields[3], "0", "first ack failed: {ack:?}");
    assert_eq!(fields[4], "0", "repeated ack must be idempotent: {ack:?}");
    wait_for_retry_loop_end(&work);
    let raw = std::fs::read(work.join("worker-raw.log")).expect("worker raw-byte log");
    assert_eq!(
        pointer_count(&raw),
        1,
        "ack did not stop retries; raw={raw:?}"
    );
    assert!(
        !orchestrator_text(&deck)
            .contains(&common::squeeze_wrapped_text("delegated worker went quiet")),
        "acknowledged worker was reported silent; grid:\n{}",
        deck.snapshot_grid()
    );
    assert_eq!(launch_pids(&work.join("worker-launches.log")).len(), 2);
}
