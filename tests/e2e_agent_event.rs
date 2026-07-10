#![cfg(feature = "e2e")]

//! L2 headless / UNATTENDED status-reporting tests for a Pi pane (PRD #201
//! M2.2, test-plan row 10).
//!
//! The flagship M2.2 requirement: a Pi pane reports `running` / `waiting` /
//! `finished` into the EXISTING `AgentEvent` stream **with no hook installed,
//! no `~/.claude/settings.json` mutation, and no TUI client attached** — the
//! workaround-dissolution of Design Decision #4.
//!
//! These drive the REAL `dot-agent-deck daemon serve` binary headlessly (the
//! `DaemonProc` harness — no PTY, no vt100 grid, no attached TUI), exactly like
//! the scheduler daemon-serve tests. The Pi extension is stood in for by the
//! SYNTHETIC harness: the real `dot-agent-deck agent-event --type <state>` CLI
//! subprocess (what the bundled extension shells), which the daemon already
//! ingests over its hook socket and re-broadcasts on the same wire every client
//! consumes. We observe status by subscribing to that broadcast the way an
//! unattended GUI / remote TUI would (`DaemonProc::subscribe_events`), and
//! derive the badge locally via `AppState::apply_event` — the identical seam
//! the production TUI's `spawn_event_subscriber` uses. All sleeping/polling
//! lives in the `common` harness (Decision 21), not in this body.
//!
//! Tier: e2e (`#[cfg(feature = "e2e")]`) because it spawns the real binary
//! (the daemon + the `agent-event` CLI). It hits NO LLM and is deterministic —
//! the daemon-serve precedents (`e2e_scheduler_*.rs`) are the model.
//!
//! GREEN-ON-WRITE: every seam this exercises already landed — the `agent-event`
//! subcommand (M1.2), `AgentType::Pi` (M1.1), the daemon's unconditional
//! raw-`AgentEvent` re-broadcast, `apply_event`'s status derivation, and the
//! fact that Claude-Code hook install (`hooks_manage::auto_install`) runs ONLY
//! at TUI/dashboard startup and is machine-global — never per-pane and never in
//! the `daemon serve` path. So spawning/handling a Pi pane installs no hook and
//! mutates no `settings.json`. This test is the regression guard that pins it.

mod common;

use std::time::Duration;

use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::state::{AppState, SessionStatus};
use spec::spec;

/// The pane the (synthetic) Pi extension reports under — the value the daemon
/// would inject as `DOT_AGENT_DECK_PANE_ID`. Chosen so it carries no capital
/// `Pi` and no hook of its own.
const PI_PANE: &str = "pi-headless-pane";
/// The registry id the daemon would inject as `DOT_AGENT_DECK_AGENT_ID`.
const PI_AGENT_ID: &str = "pi-agent-headless";

/// Scenario: Start the real `daemon serve` headlessly (no TUI client) and seed a
/// sentinel `~/.claude/settings.json` in its HOME. Standing in for the Pi
/// extension, run the real `dot-agent-deck agent-event --type running|waiting|
/// finished` CLI (which rides the existing hook socket), while an unattended
/// `SubscribeEvents` consumer watches the daemon's broadcast. Assert the daemon
/// re-broadcasts each as a bare `AgentEvent` carrying the Pi identity and the
/// mapped `EventType`, that feeding those through `AppState::apply_event`
/// (exactly as the TUI subscriber does) drives the badge Thinking →
/// WaitingForInput → Idle, and that the whole flow installs NO Claude hook and
/// leaves `~/.claude/settings.json` byte-for-byte unchanged.
#[spec("status/agent-event/003")]
#[test]
fn agent_event_003_headless_pi_status_no_hook_no_settings_mutation() {
    // Headless daemon, no schedules, idle-shutdown disabled — no TUI attaches.
    let daemon = common::spawn_daemon_serve(None, "0");

    // Seed a sentinel Claude settings.json in the daemon's HOME. Creating
    // `~/.claude/` makes `hooks_manage::auto_install`'s "does ~/.claude exist?"
    // guard PASS, so if any code path wrongly wired hook install into the
    // daemon/agent-event path, it WOULD rewrite this file — the byte-equality
    // assertion below would then fail. The sentinel deliberately contains no
    // dot-agent-deck hooks.
    let claude_dir = daemon.home.join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create per-test ~/.claude");
    let settings_path = claude_dir.join("settings.json");
    let sentinel = "{\n  \"note\": \"pi-no-hook-sentinel\"\n}\n";
    std::fs::write(&settings_path, sentinel).expect("seed sentinel settings.json");
    let before = std::fs::read(&settings_path).expect("read sentinel settings.json");

    // Subscribe as an unattended consumer BEFORE any status is reported.
    let sub = daemon.subscribe_events();

    // A local AppState is the badge sink — the same seam the production TUI's
    // event subscriber feeds. Register the pane so `apply_event` accepts the
    // (non-SessionStart) lifecycle events, exactly as the daemon/TUI register a
    // managed pane at spawn.
    let mut badge = AppState::default();
    badge.register_pane(PI_PANE.to_string());
    let session_id = format!("{PI_PANE}-session");

    // Drive the lifecycle: report each state via the real CLI, then wait for the
    // daemon to re-broadcast it, then apply it to the local badge and assert the
    // derived status. Gating each report on observing its broadcast keeps the
    // ordering deterministic (no cross-connection race).
    for (state, want_event, want_status) in [
        ("running", EventType::Thinking, SessionStatus::Thinking),
        (
            "waiting",
            EventType::WaitingForInput,
            SessionStatus::WaitingForInput,
        ),
        ("finished", EventType::Idle, SessionStatus::Idle),
    ] {
        let out = daemon.run_agent_event(PI_PANE, Some(PI_AGENT_ID), state);
        assert!(
            out.status.success(),
            "`agent-event --type {state}` must exit 0 (stderr: {})",
            String::from_utf8_lossy(&out.stderr)
        );

        let want = want_event.clone();
        let ev = sub.wait_for(
            move |e| {
                e.pane_id.as_deref() == Some(PI_PANE)
                    && e.agent_type == AgentType::Pi
                    && e.event_type == want
            },
            Duration::from_secs(10),
        );

        // The re-broadcast frame carries the Pi identity and the injected ids —
        // the daemon ingested and propagated it on the existing wire, unattended.
        assert_eq!(ev.agent_type, AgentType::Pi);
        assert_eq!(ev.pane_id.as_deref(), Some(PI_PANE));
        assert_eq!(ev.agent_id.as_deref(), Some(PI_AGENT_ID));
        assert_eq!(ev.event_type, want_event);

        // The badge a client renders follows the lifecycle.
        badge.apply_event(ev);
        let status = badge
            .sessions
            .get(&session_id)
            .unwrap_or_else(|| panic!("agent-event --type {state} created no session card"))
            .status
            .clone();
        assert_eq!(
            status, want_status,
            "after agent-event --type {state}, the unattended Pi card badge must read {want_status:?}"
        );
    }

    // Workaround-dissolution (Design Decision #4): no hook, no settings.json
    // mutation. The sentinel must be byte-identical after the whole flow, and
    // must never have gained a dot-agent-deck hook entry.
    let after = std::fs::read(&settings_path).expect("re-read settings.json");
    assert_eq!(
        after, before,
        "handling a Pi pane's status must NOT mutate ~/.claude/settings.json"
    );
    let after_str = String::from_utf8_lossy(&after);
    assert!(
        !after_str.contains("dot-agent-deck"),
        "no dot-agent-deck hook may be installed for a Pi pane; settings.json now reads:\n{after_str}"
    );

    drop(sub);
}
