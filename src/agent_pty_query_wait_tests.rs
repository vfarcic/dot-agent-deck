//! PRD #1666 part 3 (Q35-18a): the redraw estimate and `B` at profile
//! adoption, fed by the real bus, its output hook and the registry's resize,
//! nudge and hook-learned agent type.

use super::*;
use crate::composer_probe::query_ledger::QueryKind;
use crate::doorbell_screen::{CLAUDE_PROFILE, CODEX_PROFILE};
use spec::spec;

const CPR: &[u8] = b"\x1b[6n";
const FRAME: &[u8] = b"\x1b[?2026h\x1b[1;1H\x1b[Jframe";

/// A Claude Code frame at `cols`: its full-redraw marker, then the
/// full-width rules around an empty composer.
fn claude_frame(cols: u16) -> Vec<u8> {
    let rule = "─".repeat(cols as usize);
    format!(
        "\x1b[2J\x1b[H\x1b[20;1H{rule}\x1b[21;1H❯\u{a0}\x1b[22;1H{rule}\x1b[23;3Hstatus\x1b[21;3H"
    )
    .into_bytes()
}

/// `estimate` lies within the instants `waits` past each end of the window
/// `[before, after]` the feeding calls ran in.
fn within(estimate: Option<Instant>, before: Instant, after: Instant, waits: Duration) {
    let estimate = estimate.expect("an estimate");
    assert!(
        estimate >= before + waits && estimate <= after + waits,
        "{estimate:?} outside [{before:?}, {after:?}] + {waits:?}"
    );
}

/// Scenario: On an unprofiled bus, push a CPR query and a `?2026h` frame, resize it, then learn Codex's profile, and push a frame, a CPR and a frame. Before adoption there is no estimate and `B` holds through the frame; at adoption `B` still holds and the estimate counts the earlier resize's two seconds plus the margin; the first frame after adoption ends `B`, the next CPR starts it and moves the estimate, and the frame after it ends it again.
#[spec("inbox/probe/054")]
#[test]
fn probe_054_an_unprofiled_bus_adopts_codex_across_its_history() {
    let bus = AgentBus::profiled(None, 24, 80);
    bus.push(CPR.to_vec());
    bus.push(FRAME.to_vec());
    assert!(bus.input().backlog(), "the counted rule ignores the frame");
    let before = Instant::now();
    bus.resize_and_clear(24, 100, || Ok::<(), ()>(())).unwrap();
    let after = Instant::now();
    assert_eq!(bus.input().query_wait_estimate(), None, "no timing yet");
    bus.adopt_profile(&CODEX_PROFILE);
    assert!(bus.input().backlog(), "adoption answers nothing");
    within(
        bus.input().query_wait_estimate(),
        before,
        after,
        Duration::from_millis(2500),
    );
    bus.push(FRAME.to_vec());
    assert!(!bus.input().backlog(), "a frame after adoption ends it");
    let before = Instant::now();
    bus.push(CPR.to_vec());
    let after = Instant::now();
    assert!(bus.input().backlog());
    within(
        bus.input().query_wait_estimate(),
        before,
        after,
        Duration::from_millis(2500),
    );
    bus.push(FRAME.to_vec());
    assert!(!bus.input().backlog());
}

/// Scenario: Insert a registry agent with no agent type, as a wrapper-started Codex is, push a CPR query through its bus and resize its PTY; then its own hook reports Codex. The generation learns Codex's profile: `B` still holds and the next frame ends it, and the estimate counts the resize; a second report changes nothing.
#[spec("inbox/probe/054")]
#[test]
fn probe_054_a_hook_learned_codex_takes_the_frame_rule() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "probe-054-learned";
    let id = registry.insert_test_agent_for_pane(Box::new(IdleTestChild), Some(pane));
    let bus = registry.inner.lock().agents[&id].bus.clone();
    bus.push(CPR.to_vec());
    let before = Instant::now();
    registry.resize(&id, 30, 100).unwrap();
    let after = Instant::now();
    assert_eq!(bus.input().query_wait_estimate(), None);
    registry.set_agent_type_for_generation(pane, Some(&id), &AgentType::Codex);
    assert!(bus.input().backlog());
    let estimate = bus.input().query_wait_estimate();
    within(estimate, before, after, Duration::from_millis(2500));
    registry.set_agent_type_for_generation(pane, Some(&id), &AgentType::ClaudeCode);
    assert_eq!(bus.input().query_wait_estimate(), estimate, "learned once");
    bus.push(FRAME.to_vec());
    assert!(!bus.input().backlog(), "the frame rule");
}

/// Scenario: On a registry agent that learned Codex, resize its PTY through 79, 80, 79 and 80 columns and then to 80 again, and run a repaint nudge's shrink and restore. Each applied change adds two seconds to the chain; the repeated size applies nothing and adds nothing; each nudge edge adds two seconds as well.
#[spec("inbox/probe/054")]
#[test]
fn probe_054_applied_changes_and_nudge_edges_feed_the_estimate() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let pane = "probe-054-resizes";
    let id = registry.insert_test_agent_for_pane(Box::new(IdleTestChild), Some(pane));
    registry.set_agent_type_for_generation(pane, Some(&id), &AgentType::Codex);
    let input = Arc::clone(registry.inner.lock().agents[&id].bus.input());
    let (rows, cols) = {
        let inner = registry.inner.lock();
        let agent = &inner.agents[&id];
        (agent.pty_rows, agent.pty_cols)
    };
    assert_ne!(cols, 79);
    let before = Instant::now();
    for cols in [79, 80, 79, 80] {
        registry.resize(&id, rows, cols).unwrap();
    }
    let after = Instant::now();
    let four = Duration::from_millis(4 * 2000 + 500);
    within(input.query_wait_estimate(), before, after, four);
    let estimate = input.query_wait_estimate();
    registry.resize(&id, rows, 80).unwrap();
    assert_eq!(
        input.query_wait_estimate(),
        estimate,
        "no dimension changed"
    );
    assert!(registry.nudge_shrink(&id));
    registry.nudge_restore(&id);
    let after = Instant::now();
    within(
        input.query_wait_estimate(),
        before,
        after,
        four + Duration::from_millis(2 * 2000),
    );
}

/// Scenario: Resize a Claude Code-profiled bus, then push frames drawn for the new width until its evidence reads Clean, and more after that. The estimate stays where the resize put it: frames, Clean ones included, never move it.
#[spec("inbox/probe/054")]
#[test]
fn probe_054_clean_frames_leave_the_estimate() {
    let bus = AgentBus::profiled(Some(&CLAUDE_PROFILE), 24, 80);
    bus.resize_and_clear(24, 100, || Ok::<(), ()>(())).unwrap();
    let estimate = bus.input().query_wait_estimate();
    assert!(estimate.is_some());
    for _ in 0..3 {
        bus.push(claude_frame(100));
    }
    assert!(matches!(
        bus.evidence().0,
        crate::evidence_state::EvidenceStatus::Clean { .. }
    ));
    assert_eq!(bus.input().query_wait_estimate(), estimate);
    assert_eq!(bus.input().reply_class(QueryKind::Cpr), (false, false));
}
