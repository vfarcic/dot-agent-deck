#![cfg(all(feature = "e2e", unix))]

//! Synthetic PTY coverage of the attached deck's real host sampler. No real
//! agent is started, and this test is deliberately not a demo-reel clip.

mod common;

use std::time::Duration;

use common::{TuiDeck, load_scaled};
use dot_agent_deck::daemon_attach::DAEMON_START_TIMEOUT_ENV;
use spec::spec;

/// Base ceiling, before [`load_scaled`], on the launch: the deck lazy-spawning
/// its daemon and painting the empty dashboard. Equal to the harness's fixed
/// first-wait ceiling, so an idle box behaves exactly as before. Measured cause:
/// on a STARVED box (io stalled 72% of the window, load 27 on 16 CPUs) the
/// daemon's pre-bind alone took 13.6 s and the fixed 30 s expired before the
/// dashboard painted (issue #1665 has the occurrence).
const LAUNCH_READY_BASE: Duration = Duration::from_secs(30);

/// The most [`launch_ready_ceiling`] grows to. Nextest kills a test at 180 s,
/// and the deck's start bound sits [`START_BOUND_MARGIN`] under this, inside
/// the binary's own 120 s clamp, so the two ceilings stay ordered.
const LAUNCH_READY_MAX: Duration = Duration::from_secs(120);

/// How far under the readiness wait the deck's own daemon-start bound sits, so
/// a daemon that never binds still prints `daemon failed to start within …`
/// onto the grid before the wait gives up — the margin the harness default
/// keeps under its fixed 30 s.
const START_BOUND_MARGIN: Duration = Duration::from_secs(5);

/// A ceiling on something that must happen: the wait returns the instant the
/// dashboard paints, so only a deck that never comes up pays for it.
fn launch_ready_ceiling() -> Duration {
    load_scaled(LAUNCH_READY_BASE).min(LAUNCH_READY_MAX)
}

/// Scenario: Launch the real TUI with its lazily spawned daemon and no agents, then press `m` on the dashboard. The overlay must show real disk numbers, a host title and sample age; Escape must restore the empty dashboard.
#[spec("dashboard/host-metrics/004")]
#[test]
fn dashboard_host_metrics_004_real_daemon_overlay_opens_and_closes() {
    let ready_within = launch_ready_ceiling();
    let deck = TuiDeck::builder()
        .with_env(
            DAEMON_START_TIMEOUT_ENV,
            ready_within
                .saturating_sub(START_BOUND_MARGIN)
                .as_millis()
                .to_string(),
        )
        .launch_with_fixture("minimal");
    assert!(
        deck.wait_for_grid_string_within("No active agents", ready_within),
        "did not see \"No active agents\" within {ready_within:?}.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );
    deck.send_keys(b"m");
    deck.wait_until_grid("host metrics with numeric disk and age rows", |grid| {
        let numeric_row = |label: &str, unit: &str| {
            grid.lines().any(|line| {
                line.contains(label)
                    && line.contains(unit)
                    && line.chars().any(|ch| ch.is_ascii_digit())
                    && !line.contains("unknown")
            })
        };
        grid.contains("Host of this deck")
            && numeric_row("Working root", "GiB")
            && numeric_row("Worktree parent", "GiB")
            && numeric_row("Temp root", "GiB")
            && numeric_row("Sample age", "ms")
    });
    deck.send_keys(b"\x1b");
    deck.wait_until_grid("Escape restores dashboard", |grid| {
        grid.contains("No active agents")
            && !grid.contains("Host of this deck")
            && !grid.contains("Sample age")
    });
}
