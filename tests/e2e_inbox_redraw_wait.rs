#![cfg(all(feature = "e2e", unix))]

//! PRD #1666 part 3, Q35-18b (`inbox/probe/055`): O1, the doorbell's
//! evidence-refresh nudge waiting for the agent's own redraw. Through the
//! real daemon, doorbell tick and PTY, against the CPR stand-in
//! (`tests/support/cpr_composer.py`), which asks for the cursor position on
//! each size change, gets no answer, and redraws 2 s later; its profile
//! declares 2 s per size change and a 1.5 s margin. The nudges are counted
//! from the doorbell's own debug line, `doorbell evidence-refresh nudge`,
//! and timed by that line's timestamp.
//!
//! What it does not show: that a real Codex or Claude Code redraws within its
//! profile's declared time (M41), or anything about continuing activity, which
//! can keep moving the estimate (A2 §6): every case here is a finite history
//! followed by quiescence.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::*;
use serde_json::json;
use spec::spec;
use std::path::PathBuf;
use std::time::Duration;

/// The stand-in's redraw contract, as its profile declares it.
const PER_SIZE_CHANGE: Duration = Duration::from_millis(2000);
const MARGIN: Duration = Duration::from_millis(1500);
/// The doorbell's debug line for one nudge.
const NUDGE: &str = "doorbell evidence-refresh nudge";
/// Long enough for a nudge, the stand-in's 2 s redraw after it and a ring.
const RING_WAIT: Duration = Duration::from_secs(30);

/// One sandbox daemon with its log at debug for the doorbell.
struct Deck {
    daemon: common::DaemonProc,
    log: PathBuf,
    script: PathBuf,
    _bin: tempfile::TempDir,
}

impl Deck {
    fn new() -> Self {
        let redraw = &dot_agent_deck::doorbell_screen::STANDIN_CPR_PROFILE.redraw;
        assert_eq!(
            (redraw.per_size_change, redraw.margin),
            (PER_SIZE_CHANGE, MARGIN),
            "the stand-in's profile declares its contract"
        );
        let bin = common::harness_tempdir().unwrap();
        let script = bin.path().join("cpr-composer.py");
        std::fs::write(&script, include_str!("support/cpr_composer.py")).unwrap();
        let log = bin.path().join("deck.log");
        let daemon = daemon_unbound(&[
            ("DOT_AGENT_DECK_LOG", log.to_str().unwrap()),
            ("RUST_LOG", "dot_agent_deck::doorbell=debug"),
        ]);
        Deck {
            daemon,
            log,
            script,
            _bin: bin,
        }
    }

    /// Start the CPR stand-in in pane `name`, with `mode`'s variables, and
    /// report it Idle.
    fn stand_in(&self, name: &str, mode: &[(&str, &str)]) -> Pane {
        let base = self.daemon.home().join(name);
        let command = format!(
            "exec python3 '{}' '{}'",
            self.script.display(),
            base.display()
        );
        let mut env = vec![
            json!(["DOT_AGENT_DECK_TEST_COMPOSER_PROFILE", "standin-cpr"]),
            json!(["DOT_AGENT_DECK_TEST_COMPOSER_BINARY", self.script]),
        ];
        env.extend(mode.iter().map(|(k, v)| json!([k, v])));
        let pane = pane_with(
            &self.daemon,
            name,
            json!({"command": common::capability_export_command(&command), "env": env}),
        );
        status(&self.daemon, &pane, "idle", "claude-code");
        pane
    }

    /// Each nudge the doorbell sent `pane`, by its log line's timestamp.
    fn nudges(&self, pane: &Pane) -> Vec<chrono::DateTime<chrono::Utc>> {
        let agent = format!("agent_id={}", pane.agent);
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(NUDGE) && l.split_whitespace().any(|w| w == agent))
            .map(|l| {
                let stamp = l.split_whitespace().next().unwrap();
                chrono::DateTime::parse_from_rfc3339(stamp)
                    .unwrap_or_else(|e| panic!("log timestamp {stamp:?}: {e}"))
                    .with_timezone(&chrono::Utc)
            })
            .collect()
    }

    /// Resize `pane`'s PTY, as a client's terminal resize does.
    fn resize(&self, pane: &Pane, rows: u16, cols: u16) {
        let reply = attach(
            &self.daemon,
            json!({"op": "resize", "id": pane.agent, "rows": rows, "cols": cols}),
        );
        assert_eq!(reply["ok"], true, "resize: {reply}");
    }

    /// Wait for `pane` to submit its doorbell line for `id`.
    fn rings(&self, pane: &Pane, id: &str) {
        assert!(
            common::wait_until(RING_WAIT, || rung_line(pane, id).is_some()),
            "the doorbell must ring for {id}; nudges {:?}; received {:?}",
            self.nudges(pane),
            String::from_utf8_lossy(&bytes(pane))
        );
    }
}

/// Scenario: Start the CPR stand-in Idle, resize its terminal once and queue a task. The stand-in redraws by itself 2 s later, before its estimate, so the deck sends no nudge, and the doorbell then rings into the redrawn screen.
#[spec("inbox/probe/055")]
#[test]
fn probe_055_one_resize_waits_for_the_agents_own_redraw_then_rings() {
    let deck = Deck::new();
    let pane = deck.stand_in("one-resize", &[]);
    deck.resize(&pane, 40, 120);
    let id = enqueue(&deck.daemon, &pane, BODY);
    deck.rings(&pane, &id);
    assert_eq!(
        deck.nudges(&pane).len(),
        0,
        "no nudge before or with the ring"
    );
}

/// Scenario: Start the CPR stand-in Idle, resize its terminal five times back to back through 130, 129, 130, 129 and 130 columns, and queue a task. The stand-in's one redraw 2 s after the first change lands at the final width, so the deck sends no nudge and the doorbell rings.
#[spec("inbox/probe/055")]
#[test]
fn probe_055_a_burst_of_five_resizes_needs_no_nudge() {
    let deck = Deck::new();
    let pane = deck.stand_in("burst", &[]);
    for cols in [130, 129, 130, 129, 130] {
        deck.resize(&pane, 40, cols);
    }
    let id = enqueue(&deck.daemon, &pane, BODY);
    deck.rings(&pane, &id);
    assert_eq!(
        deck.nudges(&pane).len(),
        0,
        "no nudge before or with the ring"
    );
}

/// Scenario: Start a CPR stand-in that never redraws after a size change, Idle; resize its terminal once and queue a task. The deck nudges it exactly once, no earlier than 2 s plus the 1.5 s margin after the resize, and not again while its screen stays stale, so the task stays queued.
#[spec("inbox/probe/055")]
#[test]
fn probe_055_an_agent_that_never_redraws_is_nudged_once_at_its_estimate() {
    let deck = Deck::new();
    let pane = deck.stand_in("no-redraw", &[("CPR_STANDIN_NO_REDRAW", "1")]);
    let resized = chrono::Utc::now();
    deck.resize(&pane, 40, 120);
    let id = enqueue(&deck.daemon, &pane, BODY);
    assert!(
        common::wait_until(RING_WAIT, || !deck.nudges(&pane).is_empty()),
        "the nudge comes due"
    );
    let nudged = deck.nudges(&pane)[0];
    let earliest = resized + chrono::Duration::from_std(PER_SIZE_CHANGE + MARGIN).unwrap();
    assert!(
        nudged >= earliest,
        "nudged at {nudged}, before the estimate's floor {earliest}"
    );
    assert!(
        !common::wait_until(PER_SIZE_CHANGE * 2, || deck.nudges(&pane).len() > 1),
        "once per episode"
    );
    assert!(rung_line(&pane, &id).is_none(), "a stale screen isn't rung");
    state(&deck.daemon, &pane, &id, "queued");
}

/// Scenario: Start a CPR stand-in whose start-up screen gives no width witness, Idle, and queue a task with no size change since spawn. The deck nudges it once, as before O1; the stand-in redraws 2 s after the nudge's size change and the doorbell rings.
#[spec("inbox/probe/055")]
#[test]
fn probe_055_a_start_up_without_a_witness_is_nudged_once() {
    let deck = Deck::new();
    let pane = deck.stand_in("start-up", &[("CPR_STANDIN_LATE_WITNESS", "1")]);
    let id = enqueue(&deck.daemon, &pane, BODY);
    deck.rings(&pane, &id);
    assert_eq!(deck.nudges(&pane).len(), 1, "one nudge, as today");
}
