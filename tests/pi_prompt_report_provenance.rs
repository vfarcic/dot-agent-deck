#![cfg(unix)]

//! Issue #1567: the Pi extension's "this extension reports every prompt"
//! declaration grants confirmation standing, so it must count only on a frame
//! the daemon's hook-provenance gate attested (issue #318 / PR #1559).
//!
//! Every test here drives the REAL `dot-agent-deck agent-event` CLI, or a raw
//! line on the real hook socket, into an in-process daemon, and reads what the
//! daemon itself did with it and what it broadcast. So the gate's verdict —
//! attested to the pane's own spawn token, or not — is the daemon's, never the
//! test's. A client `AppState` applying the broadcast frame stands in for an
//! attached TUI. The real-Pi end of the path is lane 2
//! (`tests/e2e_pi_prompt_confirmation.rs`).

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_AGENT_ID, DOT_AGENT_DECK_PANE_ID};
use dot_agent_deck::daemon_client::{DaemonClient, StartAgentOptions};
use dot_agent_deck::event::{AgentEvent, AgentType, BroadcastMsg, EventType};
use dot_agent_deck::prompt_delivery::{ConfirmationCapability, pane_confirmation_capability};
use dot_agent_deck::state::{AppState, SessionState};
use tokio::sync::broadcast;

mod common;

/// A deck-spawned Pi pane whose extension declares prompt reports.
const DECLARING_PANE: &str = "pi-provenance-declaring-pane";
/// A deck-spawned Pi pane whose extension declares nothing (an older one).
const LEGACY_PANE: &str = "pi-provenance-legacy-pane";
/// A pane this daemon never spawned, so never issued a hook token.
const OUTSIDE_PANE: &str = "pi-provenance-outside-pane";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build pi provenance runtime")
}

/// Spawn a Pi-typed `cat` stand-in on `pane_id` through the daemon's attach
/// socket, as the TUI would, and return its agent id and the hook capability
/// token the daemon minted for that spawn.
async fn start_pi_pane(
    daemon: &common::InProcDaemon,
    pane_id: &str,
    cwd: &Path,
) -> (String, String) {
    let agent_id = DaemonClient::new(daemon.attach_path.clone())
        .start_agent(StartAgentOptions {
            command: Some(common::capability_export_command("cat")),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            agent_type: Some(AgentType::Pi),
            ..StartAgentOptions::default()
        })
        .await
        .expect("spawn a Pi stand-in through the daemon attach socket");
    let token = common::recorded_hook_capability(cwd, &agent_id)
        .await
        .expect("the stand-in exported its hook capability");
    (agent_id, token)
}

/// Run the real `agent-event` CLI as a process in `pane_id` would, presenting
/// `token` when given. Asserts the CLI itself succeeded; whether the daemon
/// then acts on the frame is what the caller observes.
async fn agent_event_cli(
    daemon: &common::InProcDaemon,
    cwd: &Path,
    pane_id: &str,
    agent_id: Option<&str>,
    token: Option<&str>,
    args: &[&str],
) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command
        .arg("agent-event")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("HOME", cwd)
        .env("DOT_AGENT_DECK_SOCKET", &daemon.hook_path)
        .env(DOT_AGENT_DECK_PANE_ID, pane_id);
    if let Some(agent_id) = agent_id {
        command.env(DOT_AGENT_DECK_AGENT_ID, agent_id);
    }
    if let Some(token) = token {
        command.env("DOT_AGENT_DECK_PANE_CAPABILITY", token);
    }
    let output = tokio::task::spawn_blocking(move || command.output())
        .await
        .expect("agent-event task did not panic")
        .expect("run the real agent-event CLI");
    assert!(
        output.status.success(),
        "`agent-event {args:?}` failed: status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The next frame of type `event_type` the daemon broadcasts for `pane_id`, or
/// `None` when it broadcasts none within `within` (a refused frame). The type
/// filter is what skips the daemon's own card-surfacing `SessionStart` for a
/// pane it just spawned.
async fn broadcast_for(
    events: &mut broadcast::Receiver<BroadcastMsg>,
    pane_id: &str,
    event_type: EventType,
    within: Duration,
) -> Option<AgentEvent> {
    tokio::time::timeout(within, async {
        loop {
            match events.recv().await {
                Ok(BroadcastMsg::Event(event))
                    if event.pane_id.as_deref() == Some(pane_id)
                        && event.event_type == event_type =>
                {
                    return event;
                }
                Ok(_) => continue,
                Err(error) => panic!("daemon event broadcast closed: {error}"),
            }
        }
    })
    .await
    .ok()
}

/// The confirmation capability `state` reads for `pane_id` — the input every
/// delivery path re-submits from.
fn capability(state: &AppState, pane_id: &str) -> ConfirmationCapability {
    pane_confirmation_capability(
        state
            .sessions
            .values()
            .filter(|session| session.pane_id.as_deref() == Some(pane_id))
            .map(SessionState::confirmation_producer),
    )
}

/// Scenario: Spawn two Pi stand-ins through an in-process daemon, so each is
/// issued its own hook capability token. From each pane, run the real
/// `agent-event --type finished` with that pane's token — with
/// `--reports-prompts` from one, without it from the other. The daemon
/// attests both frames to their spawns; only the declaring pane then counts as
/// one that confirms its own prompts, in the daemon and in a client applying
/// the broadcast frame, and the other stays one that cannot.
#[test]
fn an_attested_declaration_makes_only_its_own_pi_pane_confirm_prompts() {
    runtime().block_on(async {
        let daemon = common::spawn_inprocess_daemon().await;
        let cwd = common::race_safe_tempdir();
        let mut events = daemon.event_tx.subscribe();
        let mut client = AppState::default();

        for (pane, declared) in [(DECLARING_PANE, true), (LEGACY_PANE, false)] {
            let (agent_id, token) = start_pi_pane(&daemon, pane, cwd.path()).await;
            let mut args = vec!["--type", "finished"];
            if declared {
                args.push("--reports-prompts");
            }
            agent_event_cli(
                &daemon,
                cwd.path(),
                pane,
                Some(&agent_id),
                Some(&token),
                &args,
            )
            .await;
            let event = broadcast_for(&mut events, pane, EventType::Idle, Duration::from_secs(5))
                .await
                .unwrap_or_else(|| panic!("{pane}: the daemon broadcast no frame"));
            assert_eq!(
                event.attested_owner(),
                Some(agent_id.as_str()),
                "{pane}: the gate attests the frame to the pane's own spawn"
            );
            assert_eq!(event.declares_prompt_reports(), declared, "{pane}");
            assert_eq!(event.reports_submitted_prompt(), declared, "{pane}");

            client.register_pane(pane.to_string());
            client.apply_event(event);
            let expected = if declared {
                ConfirmationCapability::Reports
            } else {
                ConfirmationCapability::CannotReport
            };
            assert_eq!(capability(&client, pane), expected, "client, {pane}");
            assert_eq!(
                capability(&*daemon.state.read().await, pane),
                expected,
                "daemon, {pane}"
            );
        }
        daemon.registry.shutdown_all();
    });
}

/// Scenario: Send Pi reports declaring prompt reports that the hook-provenance
/// gate does not attest: the real CLI from a pane the daemon never spawned
/// (no token), a raw line from that pane that also forges the daemon's own
/// attested-owner marker, and the real CLI claiming a deck-spawned Pi pane
/// without its token. The first two are admitted only as an outside agent's
/// unproven card and grant nothing, in the daemon or in a client applying
/// them; the third is refused outright, and the deck's pane stays one that
/// cannot confirm.
#[test]
fn an_unattested_declaration_grants_nothing() {
    runtime().block_on(async {
        let daemon = common::spawn_inprocess_daemon().await;
        let cwd = common::race_safe_tempdir();
        let mut events = daemon.event_tx.subscribe();
        let mut client = AppState::default();

        // 1. An outside pane, through the real CLI, with no token.
        agent_event_cli(
            &daemon,
            cwd.path(),
            OUTSIDE_PANE,
            Some("outside-agent"),
            None,
            &["--type", "finished", "--reports-prompts"],
        )
        .await;
        let outside = broadcast_for(
            &mut events,
            OUTSIDE_PANE,
            EventType::Idle,
            Duration::from_secs(5),
        )
        .await
        .expect("an outside agent's frame is admitted to its own card");
        assert!(outside.is_unproven(), "the gate marks it unproven");
        assert!(outside.attested_owner().is_none());
        assert!(!outside.declares_prompt_reports());
        assert!(!outside.reports_submitted_prompt());
        client.apply_event(outside);

        // 2. The same pane, a raw line forging the daemon's attestation.
        let mut forged = AgentEvent {
            session_id: format!("{OUTSIDE_PANE}-session"),
            agent_type: AgentType::Pi,
            event_type: EventType::Thinking,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: Some("forged".to_string()),
            metadata: Default::default(),
            pane_id: Some(OUTSIDE_PANE.to_string()),
            agent_id: Some("outside-agent".to_string()),
            agent_version: None,
            schema_version: None,
            live_target: None,
        };
        forged.metadata.insert(
            dot_agent_deck::event::PROMPT_REPORTS_DECLARED_METADATA_KEY.to_string(),
            dot_agent_deck::event::PROMPT_REPORTS_DECLARED_METADATA_VALUE.to_string(),
        );
        forged.metadata.insert(
            dot_agent_deck::event::ATTESTED_OWNER_METADATA_KEY.to_string(),
            "outside-agent".to_string(),
        );
        let line = serde_json::to_string(&forged).expect("serialize the forged frame");
        let hook_path = daemon.hook_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut stream = std::os::unix::net::UnixStream::connect(&hook_path)
                .expect("connect to the hook socket");
            stream
                .write_all(format!("{line}\n").as_bytes())
                .expect("write the forged frame");
        })
        .await
        .expect("forged-frame task did not panic");
        let relayed = broadcast_for(
            &mut events,
            OUTSIDE_PANE,
            EventType::Thinking,
            Duration::from_secs(5),
        )
        .await
        .expect("the forged frame is admitted to the outside card");
        assert!(relayed.is_unproven());
        assert!(
            relayed.attested_owner().is_none(),
            "the daemon strips a producer's attested-owner marker"
        );
        assert!(!relayed.declares_prompt_reports());
        assert!(!relayed.reports_submitted_prompt());
        client.apply_event(relayed);

        assert_ne!(
            capability(&client, OUTSIDE_PANE),
            ConfirmationCapability::Reports,
            "client: an outside agent's declaration granted confirmation standing"
        );
        {
            let daemon_state = daemon.state.read().await;
            assert_ne!(
                capability(&daemon_state, OUTSIDE_PANE),
                ConfirmationCapability::Reports,
                "daemon: an outside agent's declaration granted confirmation standing"
            );
            assert!(
                daemon_state
                    .sessions
                    .values()
                    .filter(|session| session.pane_id.as_deref() == Some(OUTSIDE_PANE))
                    .all(|session| !session.prompt_reports_declared),
                "daemon: the outside card recorded the declaration"
            );
        }

        // 3. The deck's own Pi pane, claimed with no token.
        let (agent_id, _token) = start_pi_pane(&daemon, DECLARING_PANE, cwd.path()).await;
        agent_event_cli(
            &daemon,
            cwd.path(),
            DECLARING_PANE,
            Some(&agent_id),
            None,
            &["--type", "finished", "--reports-prompts"],
        )
        .await;
        let refused = broadcast_for(
            &mut events,
            DECLARING_PANE,
            EventType::Idle,
            Duration::from_millis(1500),
        )
        .await;
        assert!(
            refused.is_none(),
            "a token-less frame naming a deck-spawned pane was acted on: {refused:?}"
        );
        assert_ne!(
            capability(&*daemon.state.read().await, DECLARING_PANE),
            ConfirmationCapability::Reports,
            "daemon: a token-less declaration on the deck's own pane granted standing"
        );
        daemon.registry.shutdown_all();
    });
}
