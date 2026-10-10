#![cfg(feature = "e2e")]

//! Synthetic PTY coverage of the attached deck's real host sampler. No real
//! agent is started, and this test is deliberately not a demo-reel clip.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch the real TUI with its lazily spawned daemon and no agents, then press `m` on the dashboard. The overlay must show real disk numbers, a host title and sample age; Escape must restore the empty dashboard.
#[spec("dashboard/host-metrics/004")]
#[test]
fn host_metrics_004_real_daemon_overlay_opens_and_closes() {
    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
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
