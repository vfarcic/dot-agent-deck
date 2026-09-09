#![cfg(feature = "e2e")]

//! PTY-attached coverage for what the orchestrator is TOLD when a worker reports
//! `work-done` (issues #448 and #433).
//!
//! The fast-tier suite (`tests/work_done_reporting.rs`) pins the daemon's
//! decisions against in-process PTYs. This one covers the boundary that suite
//! cannot see, and that this change specifically moved: the REAL `dot-agent-deck
//! work-done` binary, over the daemon's real hook socket, into the real TUI's
//! rendered orchestration surface.
//!
//! Rendering is the point, not incidental. The old feedback was one short
//! sentence; the unsolicited label is a long paragraph carrying a framed report,
//! and a long daemon-injected line lands on the vt100 grid hard-wrapped at
//! whatever column a role pane happens to be — which is exactly how
//! `scheduler/idle-worker/011` fails today. So the assertions read the PANE
//! COLUMN and squeeze whitespace out of both sides (see [`pane_column_text`]), and
//! the test proves the label genuinely reaches a user's screen rather than merely
//! reaching a PTY.

mod common;

use std::cell::RefCell;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::TabMembership;
use spec::spec;

/// The `orch-deck` fixture's non-start `cat` role — the worker whose completion
/// this test issues. (Its `orchestrator` sibling is identified by membership, not
/// by name, in [`orchestration_ids`].)
const WORKER_ROLE: &str = "worker";

/// The #448 label, spelled out here rather than imported from `src/` so a silent
/// rewording of the daemon's template fails this test instead of following it.
const UNSOLICITED_NEEDLE: &str = "you have no outstanding delegation to that worker";

/// The daemon's provenance clause — an orchestrator agent could write prose about
/// a worker, but not a verbatim self-identification as a daemon report.
const DAEMON_CLAUSE: &str = "dot-agent-deck daemon report, not a message from a person or an agent";

/// The happy-path pointer. Its ABSENCE is the assertion: nothing was delegated,
/// so no summary file was written, so there is nothing to point at.
const POINTER_NEEDLE: &str = "Read .dot-agent-deck/work-done-worker.md for their full report.";

/// Opening marker of the untrusted-report frame the inlined report sits inside.
const REPORT_FRAME_NEEDLE: &str = "[UNTRUSTED-WORKER-REPORT:";

/// A token unique to this test's report, so its appearance on the grid proves the
/// daemon inlined THIS report. `[a-z0-9-]` only, so it survives the whitespace
/// collapse and the frame-breaking filter unchanged.
const SENTINEL: &str = "e2e-unsolicited-report-4b7d";

/// Drop every whitespace run, so a needle that straddles the pane's wrap column
/// still matches text that is fully on screen.
fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The embedded pane column's text, rows joined in order — the orchestration
/// surface as the user reads it.
///
/// Slicing the column is load-bearing, not tidiness. An orchestration tab renders
/// the role CARDS to the left of the pane on the same grid rows, so joining whole
/// rows splices card text and card borders into the middle of every wrapped pane
/// line: `…no outstanding delegat` + `┃Launch an agent to get started┃` +
/// `ion to that worker…`. A needle longer than the pane is wide then matches
/// nothing even though every character of it is on screen, which is precisely how
/// `scheduler/idle-worker/011` fails today (issue #460) — the daemon's long line
/// is plainly rendered in its dump and the assertion still cannot see it.
///
/// The pane's left border column is found from its `┌<title>` header row and is
/// constant down the box, so every row is cut at the same column and the trailing
/// border is trimmed. Char-indexed throughout: box-drawing glyphs are multibyte.
fn pane_column_text(grid: &str) -> String {
    let Some(left) = grid
        .lines()
        .find(|line| line.contains('┌'))
        .and_then(|line| line.chars().position(|c| c == '┌'))
    else {
        return String::new();
    };
    grid.lines()
        .filter_map(|line| {
            let row: Vec<char> = line.chars().collect();
            if row.len() <= left + 1 {
                return None;
            }
            // Stops at the pane's RIGHT border on content rows; on the
            // header/footer rows it yields box glyphs, which are harmless because
            // no needle contains them.
            let interior: String = row[left + 1..].iter().take_while(|c| **c != '│').collect();
            Some(interior)
        })
        .collect::<Vec<_>>()
        .join("")
}

fn pane_contains(deck: &TuiDeck, needle: &str) -> bool {
    squeeze(&pane_column_text(&deck.snapshot_grid())).contains(&squeeze(needle))
}

fn wait_for_pane_string(deck: &TuiDeck, needle: &str, timeout: Duration) -> bool {
    common::wait_until(timeout, || pane_contains(deck, needle))
}

/// The production new-pane flow: `Ctrl+n` → confirm dir → Right selects the
/// `[Orch: demo-orch]` chip → Enter → Enter. This is the only path that registers
/// the daemon-side role maps `handle_work_done` routes on.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
}

/// The worker's `pane_id_env` (what the CLI must report as) plus the
/// ORCHESTRATOR's registry agent id (what the daemon's PTY snapshot is keyed on).
///
/// Both are needed because this test asserts twice over: once that the daemon
/// WROTE the feedback into the orchestrator's PTY, and once that the TUI RENDERED
/// it. Splitting those is what makes a failure diagnosable — a daemon that never
/// composed the line and a line that never reached the grid look identical on the
/// grid alone.
fn orchestration_ids(deck: &TuiDeck) -> (String, String) {
    let ids = RefCell::new(None);
    let ready = common::wait_until(Duration::from_secs(10), || {
        let records = common::agent_records_on(deck.attach_socket_path());
        let worker = records
            .iter()
            .find_map(|record| match &record.tab_membership {
                Some(TabMembership::Orchestration { role_name, .. })
                    if role_name == WORKER_ROLE =>
                {
                    record.pane_id_env.clone()
                }
                _ => None,
            });
        let orchestrator = records.iter().find_map(|record| {
            matches!(
                &record.tab_membership,
                Some(TabMembership::Orchestration {
                    is_start_role: true,
                    ..
                })
            )
            .then(|| record.id.clone())
        });
        if let (Some(worker), Some(orchestrator)) = (worker, orchestrator) {
            *ids.borrow_mut() = Some((worker, orchestrator));
            return true;
        }
        false
    });
    assert!(
        ready,
        "the orchestration's role panes were not registered within 10s; records = {:?}",
        common::agent_records_on(deck.attach_socket_path())
    );
    ids.into_inner().expect("the ready poll stores both ids")
}

/// The orchestrator PTY's own scrollback, straight from the daemon — the bytes it
/// wrote, before any rendering is involved.
fn orchestrator_pty(deck: &TuiDeck, orchestrator_agent_id: &str) -> String {
    String::from_utf8_lossy(&common::pane_snapshot_on(
        deck.attach_socket_path(),
        orchestrator_agent_id,
    ))
    .into_owned()
}

/// Scenario: Launch the real TUI and its lazy daemon, open the two-role `orch-deck` fixture, and run the REAL `dot-agent-deck work-done` binary from the live `worker` pane without anything ever having been delegated to it — the shape of a worker a person tasked directly. The rendered orchestration surface must visibly carry the daemon's unsolicited label and the worker's own report inside its untrusted-report markers, must NOT carry the pointer to a summary file, and no `work-done-worker.md` may appear on disk.
#[spec("orchestration/work-done/004")]
#[test]
fn work_done_004_unsolicited_completion_is_visibly_labelled_in_the_attached_tui() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        // Both delegation watches off: this test is about what an UNDELEGATED
        // completion renders as, and a detector firing into the same pane would
        // be noise competing for the surface under assertion.
        .with_env("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0")
        .with_env("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0")
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");
    open_orchestration(&deck);
    deck.wait_for_string(WORKER_ROLE);

    let (worker_pane, orchestrator_agent) = orchestration_ids(&deck);
    let summary_path = deck
        .workdir()
        .join(format!(".dot-agent-deck/work-done-{WORKER_ROLE}.md"));

    // The REAL CLI, as the footer tells a worker to run it, against the deck's
    // own daemon. Nothing was delegated, so the daemon owes this pane nothing.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .arg("work-done")
        .arg("--task")
        .arg(format!("A person asked me to do this. {SENTINEL}"))
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", &worker_pane)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run the real `dot-agent-deck work-done` CLI");
    assert!(
        output.status.success(),
        "`work-done` exited {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // First: did the DAEMON compose and write the label into the orchestrator's
    // PTY at all? Asserted before the grid so a daemon-side failure is never
    // reported as a rendering failure.
    let wrote = common::wait_until(Duration::from_secs(20), || {
        squeeze(&orchestrator_pty(&deck, &orchestrator_agent))
            .contains(&squeeze(UNSOLICITED_NEEDLE))
    });
    assert!(
        wrote,
        "the daemon never wrote the unsolicited label into the orchestrator's PTY — an \
         uncommissioned completion still reads to the orchestrator as delegated work coming \
         back\nOrchestrator PTY:\n{}",
        orchestrator_pty(&deck, &orchestrator_agent)
    );

    // Then: does it reach the user's screen? A long daemon-injected line has to
    // survive the orchestration surface's wrapping to be worth anything.
    //
    // EVERY POSITIVE NEEDLE BELOW WAITS. The daemon composes one message and the
    // TUI renders whatever of it has arrived, so a grid sampled the instant the
    // FIRST needle appears can legitimately be mid-message — the label drawn and
    // the framed report not yet. Issue #818's sibling symptom: on a contended
    // runner this test failed at the report-frame assertion having PASSED the two
    // above it, which is that partial render and not a missing report. These were
    // instantaneous `pane_contains` checks; each is now a bounded wait, which
    // asserts exactly the same thing (the needle must become visible) without
    // pinning WHEN inside the message's own render. The budget is `load_scaled`
    // for the reason issue #709 gives: a fast box still fails fast.
    let visible_timeout = common::load_scaled(Duration::from_secs(20));
    assert!(
        wait_for_pane_string(&deck, UNSOLICITED_NEEDLE, visible_timeout),
        "the unsolicited label reached the orchestrator's PTY but never became visible in the \
         rendered orchestration surface\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        wait_for_pane_string(&deck, DAEMON_CLAUSE, visible_timeout),
        "the label must identify itself as a daemon report, not as a message from a person or an \
         agent\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        wait_for_pane_string(&deck, REPORT_FRAME_NEEDLE, visible_timeout)
            && wait_for_pane_string(&deck, SENTINEL, visible_timeout),
        "the worker's own report must still reach the orchestrator, framed as untrusted \
         data\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    // Deliberately NOT a wait: this is a negative window, and every needle above
    // has already settled, so the whole message is on the grid by now. Widening a
    // "must NOT appear" check would only make it slower and weaker.
    assert!(
        !pane_contains(&deck, POINTER_NEEDLE),
        "the orchestrator was pointed at a summary file that was never written — the #433 \
         defect, reached through #448's path\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        !summary_path.exists(),
        "an uncommissioned completion wrote {} — the role-keyed path is the record of \
         COMMISSIONED work and must not be overwritten by a report nobody asked for",
        summary_path.display()
    );
}
