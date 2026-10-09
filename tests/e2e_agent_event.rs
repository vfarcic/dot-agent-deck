#![cfg(all(feature = "e2e", unix))]

//! L2 headless / UNATTENDED status-reporting tests for a Pi pane (PRD #201
//! M2.2, test-plan row 10).
//!
//! The flagship M2.2 requirement: a Pi pane reports `running` / `waiting` /
//! `finished` into the EXISTING `AgentEvent` stream **with no hook installed,
//! no `~/.claude/settings.json` mutation, and no TUI client attached** — the
//! workaround-dissolution of Design Decision #4.
//!
//! These drive the REAL `dot-agent-deck daemon serve` binary headlessly (the
//! `DaemonProc` harness — no vt100 grid, no attached TUI), exactly like the
//! scheduler daemon-serve tests. The Pi pane is a SYNTHETIC stand-in the daemon
//! spawns itself, which runs the real `dot-agent-deck agent-event --type
//! <state>` CLI from inside the pane (what the bundled extension shells), so
//! each report carries the pane's injected ids and hook capability token
//! (issue #318) exactly as a real Pi pane's does. The daemon ingests it over
//! its hook socket and re-broadcasts it on the same wire every client
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
//! subcommand (M1.2), `AgentType::Pi` (M1.1), the daemon's attested
//! `AgentEvent` re-broadcast, `apply_event`'s status derivation, and the
//! fact that Claude-Code hook install (`hooks_manage::auto_install`) runs ONLY
//! at TUI/dashboard startup and is machine-global — never per-pane and never in
//! the `daemon serve` path. So spawning/handling a Pi pane installs no hook and
//! mutates no `settings.json`. This test is the regression guard that pins it.

mod common;

use std::time::Duration;

use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::{AgentType, EventType};
use dot_agent_deck::state::{AppState, SessionState, SessionStatus};
use dot_agent_deck::ui::{CardDensityKind, render_card_to_buffer};
use spec::spec;

/// The pane the (synthetic) Pi extension reports under — the value the TUI
/// passes as `DOT_AGENT_DECK_PANE_ID` when it asks the daemon to spawn the pane.
/// Chosen so it carries no capital `Pi` and no hook of its own.
const PI_PANE: &str = "pi-headless-pane";

/// The stand-in for a Pi pane: a shell that, like the bundled extension, runs
/// `dot-agent-deck agent-event --type <state>` from inside the pane for each
/// state typed into it. It inherits everything the daemon injects at spawn —
/// pane id, agent id, hook socket and the pane's hook capability token — and
/// adds nothing of its own. `PI_EXT_BIN` is the binary under test.
const PI_STAND_IN: &str = "sh -c 'while IFS= read -r s; do
    case \"$s\" in
        prompt) \"$PI_EXT_BIN\" agent-event --type prompt --cwd /work/pi-detail --prompt \"list pi-detail-sentinel\" ;;
        tool-start) \"$PI_EXT_BIN\" agent-event --type tool-start --cwd /work/pi-detail --tool-name bash --tool-detail \"ls pi-detail-sentinel\" ;;
        tool-end) \"$PI_EXT_BIN\" agent-event --type tool-end --cwd /work/pi-detail --tool-name bash ;;
        *) \"$PI_EXT_BIN\" agent-event --type \"$s\" ;;
    esac
done'";

/// Render the received session through the same card widget as the dashboard.
fn rendered_card(session: &SessionState, now: chrono::DateTime<chrono::Utc>) -> String {
    let density = CardDensityKind::Normal;
    let buffer = render_card_to_buffer(
        session,
        None,
        Some(1),
        density,
        0,
        now,
        false,
        100,
        density.rendered_height(),
    );
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

/// Scenario: Start the real daemon headlessly with its default enforce policy
/// and a Pi stand-in pane that runs `agent-event` with the pane's inherited token.
/// Report lifecycle states plus a prompt and tool call with their directory,
/// then render the received card to confirm its status and details; send the
/// same detail event types from outside without a token and confirm they never
/// reach the subscriber or change the card.
/// The flow leaves a seeded Claude settings file unchanged and installs no hook.
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

    // The Pi pane is the daemon's own spawn, as every pane a TUI or the desktop
    // registers is: the daemon mints its hook capability token at spawn, and
    // the CLI inside the pane presents it with every report. A report sent from
    // OUTSIDE the pane carries none, so the default enforce policy refuses
    // that report when it claims this daemon-managed pane (issue #318).
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let response = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some(PI_STAND_IN.into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![
                ("DOT_AGENT_DECK_PANE_ID".into(), PI_PANE.into()),
                ("PI_EXT_BIN".into(), bin.into()),
            ],
            display_name: None,
            tab_membership: None,
            agent_type: Some(AgentType::Pi),
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        })
        .expect("StartAgent over the attach socket");
    assert!(
        response.error.is_none(),
        "StartAgent for the Pi stand-in must succeed, got error: {:?}",
        response.error
    );
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(records.len(), 1, "the Pi stand-in must be registered");
    let pi_agent_id = records[0].id.clone();

    // A local AppState is the badge sink — the same seam the production TUI's
    // event subscriber feeds. Register the pane so `apply_event` accepts the
    // (non-SessionStart) lifecycle events, exactly as the TUI registers the pane
    // the daemon just spawned for it.
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
        assert!(
            daemon.send_pane_input(&pi_agent_id, &format!("{state}\r")),
            "typing `{state}` into the Pi stand-in pane must reach its PTY"
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
        assert_eq!(ev.agent_id.as_deref(), Some(pi_agent_id.as_str()));
        assert_eq!(ev.event_type, want_event);
        // The pane's token attested the report: the daemon relays it as its own
        // agent's, not an outside agent's.
        assert!(
            !ev.is_unproven(),
            "the Pi pane's own `agent-event --type {state}` must reach clients as proven"
        );

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

    // Detail-bearing types use the same inherited token as lifecycle reports.
    // The tool-start and prompt are both observed on the rendered card, rather
    // than merely inspecting the frame or the client's routing bookkeeping.
    for (kind, want_event, want_status, detail) in [
        (
            "prompt",
            EventType::Thinking,
            SessionStatus::Thinking,
            "list pi-detail-sentinel",
        ),
        (
            "tool-start",
            EventType::ToolStart,
            SessionStatus::Working,
            "ls pi-detail-sentinel",
        ),
        (
            "tool-end",
            EventType::ToolEnd,
            SessionStatus::Working,
            "Tools: 1",
        ),
    ] {
        assert!(daemon.send_pane_input(&pi_agent_id, &format!("{kind}\r")));
        let ev = sub.wait_for(
            |e| {
                e.pane_id.as_deref() == Some(PI_PANE)
                    && e.event_type == want_event
                    && e.cwd.as_deref() == Some("/work/pi-detail")
            },
            Duration::from_secs(10),
        );
        assert!(!ev.is_unproven(), "in-pane {kind} must be attested");
        assert_eq!(ev.agent_type, AgentType::Pi);
        assert_eq!(ev.agent_id.as_deref(), Some(pi_agent_id.as_str()));
        badge.apply_event(ev);
        let card = badge.sessions.get(&session_id).expect("Pi card");
        assert_eq!(card.status, want_status);
        let now = chrono::Utc::now();
        let before = rendered_card(card, now);
        assert!(
            before
                .lines()
                .any(|line| line.contains("Dir:") && line.contains("pi-detail")),
            "card directory basename:\n{before}"
        );
        assert!(before.contains(detail), "card {kind} detail:\n{before}");
        assert!(
            before.contains("list pi-detail-sentinel"),
            "card keeps the submitted prompt:\n{before}"
        );
        if kind == "tool-start" {
            assert!(before.contains("bash"), "card active tool name:\n{before}");
        }

        // Reuse the real CLI, pane id and agent id OUTSIDE the pane, explicitly
        // removing this test worker's own ambient capability. The distinctive
        // forged detail distinguishes this report from the earlier proven one.
        let forged = std::process::Command::new(bin)
            .args(["agent-event", "--type", kind, "--cwd", "/outside-forged"])
            .args(if kind == "prompt" {
                vec!["--prompt", "FORGED-PI-PROMPT"]
            } else {
                vec![
                    "--tool-name",
                    "FORGED-PI-TOOL",
                    "--tool-detail",
                    "FORGED-PI-DETAIL",
                ]
            })
            .env("HOME", &daemon.home)
            .env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket)
            .env("DOT_AGENT_DECK_PANE_ID", PI_PANE)
            .env("DOT_AGENT_DECK_AGENT_ID", &pi_agent_id)
            .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY")
            .output()
            .expect("run outside agent-event without a token");
        assert!(
            forged.status.success(),
            "outside {kind} CLI failed: {}",
            String::from_utf8_lossy(&forged.stderr)
        );
        let outside = sub.try_wait_for(
            |e| {
                e.pane_id.as_deref() == Some(PI_PANE)
                    && e.event_type == want_event
                    && e.cwd.as_deref() == Some("/outside-forged")
            },
            Duration::from_millis(500),
        );
        // Enforce rejects a tokenless report claiming a managed pane before
        // fan-out, so the card subscriber must receive no such frame.
        if let Some(outside) = outside {
            badge.apply_event(outside);
            let after = rendered_card(badge.sessions.get(&session_id).expect("Pi card"), now);
            panic!(
                "outside {kind} reached the card subscriber; card before:\n{before}\ncard after:\n{after}"
            );
        }
        assert_eq!(
            rendered_card(badge.sessions.get(&session_id).expect("Pi card"), now),
            before,
            "an outside {kind} without the pane's token must not change its card"
        );
        assert_eq!(
            badge.sessions.len(),
            1,
            "no duplicate outside card for the pane"
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
