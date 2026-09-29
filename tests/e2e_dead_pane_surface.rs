#![cfg(all(feature = "e2e", unix))]

//! PTY-attached coverage for a pane whose agent goes away underneath it.
//!
//! When the attach I/O task exhausts its reconnect budget the pane can never
//! accept input again, but it keeps rendering its last frame — so without a
//! marker it is indistinguishable from an agent that is merely quiet, and every
//! keystroke is silently dropped. A maintainer hit exactly this on v0.35.0 and
//! read it as the deck freezing; the only feedback was a transient
//! `PTY write failed: Pane <id> stream I/O task ended`, naming an internal task.
//!
//! Driven through the real binary on a PTY because the whole defect is about
//! what a user SEES: the rendered title and the message a keystroke produces.
//! Neither is observable from the state layer, and the unit tests covering
//! `PaneLostReason`'s strings cannot reach this path at all.

mod common;

use std::time::Duration;

use common::TuiDeck;

/// Room for the give-up to happen and repaint: the reattach lookup runs for
/// `REATTACH_LOOKUP_TOTAL_BUDGET` (2 × `RESPAWN_SLOT_HANDOVER_WORST_CASE`, so
/// 10s) before the task concludes no live agent will claim the pane. The default
/// `WAIT_TIMEOUT` is exactly 10s, which would race the thing under test.
const GIVE_UP_BUDGET: Duration = Duration::from_secs(25);

/// Where the agent writes its own pid, relative to the deck's workdir (the pane's
/// cwd). `$$` of the `sh` that then `exec`s `cat`, so it is `cat`'s pid.
const AGENT_PID_FILE: &str = "orphan-target.pid";

/// `(ppid, args)` of `pid`, via `ps` so it holds on every Unix, not only where
/// `/proc` exists.
fn parent_and_args(pid: i32) -> Option<(i32, String)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (ppid, args) = line.split_once(char::is_whitespace)?;
    Some((ppid.trim().parse().ok()?, args.trim().to_string()))
}

/// The pid of the `daemon serve` process that owns `agent_pid`, found by walking
/// UP from that agent. Deliberately not a `pkill`-style pattern match: every deck
/// on the box shares that command line (CLAUDE.md rule 12's teardown step), and
/// only an ancestor of this test's own agent is this test's daemon.
fn owning_daemon_pid(agent_pid: i32) -> Option<i32> {
    // The agent is the daemon's child, or its grandchild when the command is
    // wrapped in the default shell. A few ancestors is ample; the bound stops a
    // walk that missed the daemon from climbing all the way to init.
    std::iter::successors(Some(agent_pid), |&pid| {
        parent_and_args(pid)
            .map(|(ppid, _)| ppid)
            .filter(|&ppid| ppid > 1)
    })
    .skip(1)
    .take(4)
    .find(|&pid| {
        parent_and_args(pid).is_some_and(|(_, args)| {
            args.contains("dot-agent-deck") && args.contains("daemon serve")
        })
    })
}

/// Scenario: Launch the deck with one pane backed by `cat`, then SIGKILL the
/// daemon that owns it — the agent vanishes underneath a pane the TUI still
/// holds, with no close initiated or announced by any client. The attach task
/// retries, finds no live agent for the pane, and gives up. Assert the pane's
/// title then reports `disconnected` rather than continuing to look healthy, and
/// that typing into it explains the agent is gone instead of naming an internal
/// I/O task.
#[test]
fn dead_pane_reports_itself_as_disconnected_and_says_why() {
    let deck = TuiDeck::builder()
        .with_continue_session(
            "orphan-target",
            format!("sh -c 'echo $$ > {AGENT_PID_FILE}; exec cat'"),
        )
        .launch_with_fixture("minimal");

    // The pane view is up and healthy: its title carries the session name and
    // NOT the marker. Asserting absence first makes the later wait a genuine
    // absent→present transition rather than a vacuous match.
    deck.wait_for_string("orphan-target");
    let healthy = deck.snapshot_grid();
    assert!(
        !healthy.contains("disconnected"),
        "a live pane must not be labelled disconnected.\nGrid:\n{healthy}"
    );

    // Take the agent away from underneath the pane by killing the daemon that
    // holds it — the shape of a daemon crash or an OOM kill, which nobody asked
    // for and nothing announces.
    //
    // This used to be `StopAgent` over the attach socket, and that is no longer
    // an unannounced death (PRD #1223, `d441cabb`): a stop is now an explicit
    // close the daemon BROADCASTS to every attached TUI as a pane-closed
    // `SessionEnd`, and the TUI correctly removes the pane instead of keeping a
    // dead one (`newagent/visibility/003`). Killing only the agent's process is
    // not this path either: the daemon keeps a naturally-exited agent registered
    // and marked crashed (issue #868), so the pane reports "the agent has
    // exited" and stays restartable rather than disconnected. What remains for
    // this surface is an agent the daemon no longer has at all, unannounced.
    let pid_file = deck.workdir().join(AGENT_PID_FILE);
    let read_pid = || {
        std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
    };
    assert!(
        common::wait_until(Duration::from_secs(10), || read_pid().is_some()),
        "the agent must record its pid at {}",
        pid_file.display()
    );
    let agent_pid = read_pid().expect("the pid file parsed just above");
    let daemon_pid = owning_daemon_pid(agent_pid).unwrap_or_else(|| {
        panic!("no `daemon serve` ancestor found for this pane's agent (pid {agent_pid})")
    });
    // SAFETY: kill(2) on one specific pid — this test's own daemon, found as an
    // ancestor of the agent this test launched.
    let rc = unsafe { libc::kill(daemon_pid, libc::SIGKILL) };
    assert_eq!(rc, 0, "SIGKILL of daemon pid {daemon_pid} failed");

    // The give-up must become visible on its own, with no keystroke to provoke
    // it — that is the whole point. Before this fix the pane sat there looking
    // live until the user typed and got an internal error.
    assert!(
        deck.wait_for_grid_string_within("disconnected", GIVE_UP_BUDGET),
        "after its agent went away the pane must label itself disconnected \
         without being prodded.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // The session name survives alongside the marker: the frozen output is kept
    // for inspection, so the pane must remain identifiable rather than being
    // replaced by a bare error.
    let disconnected = deck.snapshot_grid();
    assert!(
        disconnected.contains("orphan-target"),
        "a disconnected pane must stay identifiable — its output is preserved \
         precisely so it can still be read.\nGrid:\n{disconnected}"
    );

    // Typing must explain the state in the user's terms. `AgentGone` is the
    // expected reason: no live agent ever claimed the pane within the retry
    // window, which is what losing the daemon produces.
    deck.send_keys(b"x");
    assert!(
        deck.wait_for_grid_string_within("no longer running", GIVE_UP_BUDGET),
        "typing into a disconnected pane must say the agent is gone.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // The internal phrasing must not come back. This is the exact string the
    // maintainer saw and could not act on.
    let after_typing = deck.snapshot_grid();
    assert!(
        !after_typing.contains("stream I/O task ended"),
        "the internal I/O-task message must not reach the user.\nGrid:\n{after_typing}"
    );
}
