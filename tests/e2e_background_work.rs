#![cfg(all(feature = "e2e", unix))]

//! Synthetic hooks and real detached processes against the daemon and TUI.
#[path = "common/background_work.rs"]
mod background_work;
mod common;

use background_work::{Fixture, PANE};
use dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID;
use dot_agent_deck::daemon_client::StartAgentOptions;
use dot_agent_deck::event::{AgentType, EventType};
use serde_json::json;
use spec::spec;
use std::time::Duration;

/// Scenario: Send genuine-shaped Claude start/Stop hooks to a daemon-owned stand-in, then keep a real detached command alive. The card stays Idle with one background command, clears when it ends, and hides the aggregate during a new turn; a pane without hook reports retains legacy shell promotion.
#[spec("status/background/001")]
#[test]
fn background_001_reporting_turn_stays_idle_while_command_runs() {
    let f = Fixture::new(AgentType::ClaudeCode);
    f.hook("claude-code", "SessionStart", json!({"source": "startup"}));
    f.hook("claude-code", "Stop", json!({}));
    f.command("one", true);
    f.wait_commands(1);
    assert_eq!(
        f.live()["status"],
        json!("Idle"),
        "sampled background command cannot promote ended turn"
    );
    assert!(
        f.deck
            .wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid.contains("Idle")
                && grid.contains("Background: 1 command")),
        "Idle and background count must be visible:\n{}",
        f.deck.snapshot_grid()
    );
    let oldest = f.live()["background"]["since_ms"].clone();
    assert!(
        oldest.is_i64(),
        "sampled command has a daemon first-seen instant"
    );
    f.hook(
        "claude-code",
        "UserPromptSubmit",
        json!({"prompt": "new turn"}),
    );
    assert!(
        f.live()["background"].is_null(),
        "live turn hides previously sampled work"
    );
    let ended = f.hook("claude-code", "Stop", json!({}));
    assert_eq!(
        ended["background"]["commands"],
        json!(1),
        "Stop must broadcast its post-transition aggregate immediately"
    );
    assert_eq!(
        ended["background"]["since_ms"], oldest,
        "new foreground turn must not restart the sampled command's age"
    );
    f.command("one", false);
    f.wait_commands(0);
    assert!(
        f.live()["background"].is_null(),
        "empty snapshot aggregate must be None"
    );
    f.events.wait_for(
        |event| {
            event.agent_id.as_deref() == Some(&f.agent_id)
                && event.event_type == EventType::ShellIdle
                && serde_json::to_value(event).unwrap()["background"]["commands"] == json!(0)
                && serde_json::to_value(event).unwrap()["background_revision"]
                    .as_u64()
                    .is_some_and(|revision| {
                        revision > ended["background_revision"].as_u64().unwrap()
                    })
        },
        Duration::from_secs(10),
    );
    assert!(
        f.deck
            .wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid.contains("Idle")
                && !grid.contains("Background:"))
    );

    // Legacy reducer control: no genuine report and no aggregate field.
    let mut legacy = dot_agent_deck::state::AppState::default();
    legacy.register_pane("legacy-pane".into());
    let busy = serde_json::from_value(json!({"session_id": "legacy-session", "pane_id": "legacy-pane", "agent_type": "none", "event_type": "shell_busy", "timestamp": chrono::Utc::now()})).unwrap();
    legacy.apply_event(busy);
    assert_eq!(
        legacy.sessions["legacy-session"].status,
        dot_agent_deck::state::SessionStatus::Working
    );
}

/// Scenario: Replay paired Claude and Codex subagent hooks around a main Stop, including duplicate starts/stops and three unpaired internal stops. Counts and oldest receipt time follow only live ids, bounded at 128; empty/oversized ids stay subagent-attributed without becoming main-thread tools, and a new conversation clears the work.
#[spec("status/background/002")]
#[test]
fn background_002_subagent_pairing_bounds_and_codex_hooks() {
    for (agent, kind) in [
        ("claude-code", AgentType::ClaudeCode),
        ("codex", AgentType::Codex),
    ] {
        let f = Fixture::new(kind);
        f.hook(agent, "SessionStart", json!({"source": "startup"}));
        let receipt_floor = chrono::Utc::now().timestamp_millis();
        f.hook(
            agent,
            "SubagentStart",
            json!({"agent_id": "A", "agent_type": "general-purpose"}),
        );
        f.hook(
            agent,
            "SubagentStart",
            json!({"agent_id": "B", "agent_type": "general-purpose"}),
        );
        assert!(
            f.live()["background"].is_null(),
            "work during the turn is not background"
        );
        let ended = f.hook(agent, "Stop", json!({}));
        assert_eq!(
            ended["background"]["subagents"],
            json!(2),
            "Stop's broadcast must contain the post-transition count"
        );
        let original = f.assert_subagents(2);
        let since = original["background"]["since_ms"].as_i64().unwrap();
        assert!(
            since >= receipt_floor && since <= chrono::Utc::now().timestamp_millis(),
            "age uses daemon receipt time"
        );
        for id in ["internal-1", "internal-2", "internal-3"] {
            f.hook(agent, "SubagentStop", json!({"agent_id": id}));
            assert_eq!(f.assert_subagents(2)["background"], original["background"]);
        }
        f.hook(agent, "SubagentStart", json!({"agent_id": "A"}));
        assert_eq!(
            f.assert_subagents(2)["background"],
            original["background"],
            "duplicate start preserves original time"
        );
        f.hook(agent, "SubagentStop", json!({"agent_id": "A"}));
        let remaining = f.assert_subagents(1);
        assert!(
            remaining["background"]["since_ms"].as_i64().unwrap() > since,
            "removing the oldest item must recompute the age from B"
        );
        f.hook(agent, "SubagentStop", json!({"agent_id": "A"}));
        assert_eq!(f.assert_subagents(1)["background"], remaining["background"]);
        f.hook(agent, "SubagentStop", json!({"agent_id": "B"}));
        f.assert_subagents(0);
        for id in [String::new(), "x".repeat(257), "é".repeat(129)] {
            f.hook(agent, "SubagentStart", json!({"agent_id": id}));
            let tool = f.hook(agent, "PreToolUse", json!({"agent_id": id, "tool_name": "Bash", "tool_input": {"command": "echo bounded"}}));
            assert!(
                tool["metadata"]
                    .get(dot_agent_deck::event::SUBAGENT_ID_METADATA_KEY)
                    .is_some(),
                "invalid tracker id must retain subagent attribution"
            );
            f.assert_subagents(0);
            assert_eq!(
                f.live()["status"],
                json!("Idle"),
                "invalid tracker id is never a main-thread tool"
            );
        }
        for n in 0..129 {
            f.hook(
                agent,
                "SubagentStart",
                json!({"agent_id": format!("bounded-{n}")}),
            );
        }
        f.assert_subagents(128);
        for n in 0..128 {
            f.hook(
                agent,
                "SubagentStop",
                json!({"agent_id": format!("bounded-{n}")}),
            );
        }
        f.assert_subagents(0);
        f.hook(agent, "SubagentStart", json!({"agent_id": "before-clear"}));
        f.hook(
            agent,
            "SessionStart",
            json!({"source": "clear", "session_id": "background-cleared-session"}),
        );
        f.assert_subagents(0);
    }
}

/// Scenario: Send the payloads the deck's OpenCode plugin sends for a root session that starts a subagent's child session, through the real `hook --agent opencode` CLI. The child's start and tool call leave the live turn's status alone; after the root goes idle the card reads Idle with `Background: 1 subagent`, the child going busy again does not double-count, and the child going idle clears the line while the card stays Idle.
#[spec("status/background/010")]
#[test]
fn background_010_opencode_child_session_counts_as_a_subagent() {
    let f = Fixture::new(AgentType::OpenCode);
    f.opencode("session.created", json!({}));
    f.opencode(
        "session.prompt",
        json!({"prompt": "delegate the slow work"}),
    );
    f.opencode("subagent.start", json!({"subagent_id": "child-1"}));
    let tool = f.opencode(
        "tool.execute.before",
        json!({"subagent_id": "child-1", "tool_name": "bash", "tool_input": {"command": "sleep 60"}}),
    );
    assert!(
        tool["metadata"]
            .get(dot_agent_deck::event::SUBAGENT_ID_METADATA_KEY)
            .is_some(),
        "a child session's tool call is its subagent's: {tool}"
    );
    let live = f.live();
    assert_eq!(
        live["status"],
        json!("Thinking"),
        "a child session's start and tool call do not move the live turn's status: {live}"
    );
    assert!(
        live["background"].is_null(),
        "work during the turn is not background: {live}"
    );

    let ended = f.opencode("session.status", json!({"status": "idle"}));
    assert_eq!(
        ended["background"]["subagents"],
        json!(1),
        "the root's idle broadcast carries the running child: {ended}"
    );
    f.assert_subagents(1);
    assert_eq!(f.live()["status"], json!("Idle"));
    assert!(
        f.deck
            .wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid.contains("Idle")
                && grid.contains("Background: 1 subagent")),
        "Idle and the background subagent must be visible:\n{}",
        f.deck.snapshot_grid()
    );
    let original = f.live()["background"].clone();

    f.opencode("subagent.start", json!({"subagent_id": "child-1"}));
    f.opencode(
        "tool.execute.before",
        json!({"subagent_id": "child-1", "tool_name": "bash", "tool_input": {"command": "ls"}}),
    );
    assert_eq!(
        f.assert_subagents(1)["background"],
        original,
        "the same child going busy again is not a second subagent"
    );
    assert_eq!(
        f.live()["status"],
        json!("Idle"),
        "a background child's tool call does not turn the ended turn Working"
    );

    f.opencode("subagent.stop", json!({"subagent_id": "child-1"}));
    f.assert_subagents(0);
    assert_eq!(f.live()["status"], json!("Idle"));
    assert!(
        f.deck
            .wait_for_grid_predicate_within(Duration::from_secs(10), |grid| grid.contains("Idle")
                && !grid.contains("Background:")),
        "the line clears when the child goes idle:\n{}",
        f.deck.snapshot_grid()
    );
}

/// Scenario: Change a real stand-in pane's detached command count from one to two to one while its completed turn stays Idle. Each count change reaches subscribers once, identical Idle+busy samples do not spam events, and retiring the spawn while a command is live cannot transfer the old observation to a successor in that pane.
#[spec("status/background/006")]
#[test]
fn background_006_monitor_count_edges_no_idle_spam_and_replacement() {
    let mut f = Fixture::new(AgentType::ClaudeCode);
    f.hook("claude-code", "SessionStart", json!({"source": "startup"}));
    f.hook("claude-code", "Stop", json!({}));
    for (name, enabled, count) in [("one", true, 1), ("two", true, 2), ("two", false, 1)] {
        let before = f.events.snapshot().len();
        f.command(name, enabled);
        f.wait_commands(count);
        f.wait_count_event(before, count);
    }
    f.assert_no_repeated_shell_events();
    f.runtime
        .block_on(f.client.stop_agent(&f.agent_id))
        .unwrap();
    let before = f.events.snapshot().len();
    f.agent_id = f
        .runtime
        .block_on(f.client.start_agent(StartAgentOptions {
            command: Some(common::capability_export_command("cat")),
            cwd: Some(f.deck.workdir().to_string_lossy().into_owned()),
            display_name: Some("successor".into()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.into(), PANE.into())],
            agent_type: Some(AgentType::ClaudeCode),
            ..Default::default()
        }))
        .unwrap();
    f.capability = f
        .runtime
        .block_on(common::recorded_hook_capability(
            f.deck.workdir(),
            &f.agent_id,
        ))
        .unwrap();
    f.hook("claude-code", "SessionStart", json!({"source": "startup"}));
    f.hook("claude-code", "Stop", json!({}));
    f.assert_successor_clean(before);
}
