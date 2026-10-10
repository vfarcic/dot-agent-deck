#![cfg(all(feature = "e2e", unix))]

//! Persistent hook-binary notices over the real daemon and attached dashboard.
mod common;
#[path = "support/hook_stale.rs"]
mod hook_stale;

use std::time::Duration;

use common::{TuiDeck, write_hook_line};
use dot_agent_deck::hook_binary::HookBinaryReason;
use hook_stale::{StaleHome, notices};
use spec::spec;

fn event(event_type: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": "stale1637", "pane_id": "stale-pane-1637",
        "agent_type": "claude_code", "event_type": event_type,
        "timestamp": "2026-10-10T12:00:00Z"
    })
}

/// Scenario: Start the real dashboard with Claude hooks pinned to an older
/// stub. Its startup notice names the pin and version; a stamp-less hook then
/// updates the same visible row live to explain that version reporting is missing,
/// even when the host has no deck installed on PATH.
#[spec("hooks/stale/002")]
#[test]
fn hooks_stale_002_startup_notice_updates_live_for_an_unreported_hook() {
    let fixture = StaleHome::new();
    // Keep the host's installed deck and login-shell profile out of the
    // scenario; the fixture supplies its own installed link to the cargo build.
    let deck = TuiDeck::builder()
        .with_pty_size(420, 28)
        .with_env("HOME", fixture.home.to_string_lossy())
        .with_env("PATH", fixture.home.to_string_lossy())
        .with_env("SHELL", "")
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    deck.wait_for_string("0.0.1");
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains(fixture.pin.to_str().unwrap()),
        "pin missing:\n{grid}"
    );
    assert!(grid.contains("Claude Code"), "agent missing:\n{grid}");
    let rows: Vec<_> = grid.lines().collect();
    let notice_row = rows.iter().position(|r| r.contains("0.0.1")).unwrap();
    let experimental_row = rows
        .iter()
        .position(|r| r.contains("experimental: on"))
        .unwrap();
    assert!(
        notice_row < experimental_row,
        "notice belongs above experimental footer:\n{grid}"
    );
    assert!(
        notices(deck.attach_socket_path())
            .iter()
            .any(|n| n.binary == fixture.pin.to_string_lossy()
                && n.reason == HookBinaryReason::Older)
    );

    write_hook_line(deck.hook_socket_path(), &event("session_start").to_string()).unwrap();
    deck.wait_for_string("stale1637");
    let updated = notices(deck.attach_socket_path());
    assert!(
        updated
            .iter()
            .any(|n| n.binary == fixture.pin.to_string_lossy()
                && n.reason == HookBinaryReason::Unreported),
        "Hello must report the processed stamp-less event: {updated:?}"
    );
    deck.wait_for_string("predates version reporting");
    assert!(deck.snapshot_grid().contains(fixture.pin.to_str().unwrap()));
}

/// Scenario: Start a headless daemon with an old stub pin and send several
/// identical stamp-less hooks. Hello reports the updated notice, while the log
/// warns once for each reason, without relying on a deck installed on the host PATH.
#[spec("hooks/stale/003")]
#[test]
fn hooks_stale_003_hello_reports_notices_and_warnings_are_deduplicated() {
    let fixture = StaleHome::new();
    let log = fixture.home.join("deck.log");
    // Match a CI runner without any host deck or login-shell PATH to fall back to.
    let daemon = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("HOME", fixture.home.to_str().unwrap()),
            ("PATH", fixture.home.to_str().unwrap()),
            ("SHELL", ""),
            ("DOT_AGENT_DECK_LOG", log.to_str().unwrap()),
        ],
    );
    let initial = notices(&daemon.attach_socket);
    assert!(
        initial
            .iter()
            .any(|n| n.binary == fixture.pin.to_string_lossy()
                && n.reason == HookBinaryReason::Older),
        "startup Hello: {initial:?}\ndaemon log:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let events = daemon.subscribe_events();
    write_hook_line(&daemon.hook_socket, &event("session_start").to_string()).unwrap();
    let repeated = format!("{}\n", event("tool_start")).repeat(5);
    write_hook_line(&daemon.hook_socket, repeated.trim_end()).unwrap();
    write_hook_line(&daemon.hook_socket, &event("idle").to_string()).unwrap();
    events.wait_for(
        |e| e.session_id == "stale1637" && e.event_type == dot_agent_deck::event::EventType::Idle,
        Duration::from_secs(10),
    );
    let updated = notices(&daemon.attach_socket);
    assert!(
        updated
            .iter()
            .any(|n| n.binary == fixture.pin.to_string_lossy()
                && n.reason == HookBinaryReason::Unreported),
        "updated Hello: {updated:?}"
    );
    let log = std::fs::read_to_string(&log).expect("daemon log");
    for reason in ["Older", "Unreported"] {
        let count = log
            .lines()
            .filter(|line| {
                line.contains("agent hooks run a dot-agent-deck")
                    && line.contains(fixture.pin.to_str().unwrap())
                    && line.contains(reason)
            })
            .count();
        assert_eq!(count, 1, "expected one warning for {reason}:\n{log}");
    }
}
