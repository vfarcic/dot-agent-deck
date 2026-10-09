//! The daemon refuses an orchestration membership that carries no per-tab
//! `orchestration_id` — issue #463.
//!
//! PRD #140 (v0.35.0) gave every orchestration tab a per-tab instance token so
//! two tabs of one orchestration in one directory are two routing groups. A
//! client older than that sends none, and the daemon used to route its panes on
//! the `(name, cwd)` tuple instead — the one identity that cannot keep two such
//! tabs apart. Those clients are no longer supported, and the daemon ignores
//! `client_version`, so it is the daemon that has to refuse: a client-side check
//! would not stop an old TUI.
//!
//! This drives the REAL `StartAgent` handler over the attach socket, as a TUI or
//! the desktop does, against an in-process daemon. No LLM and no TUI.

#![cfg(unix)]

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, TabMembership};
use dot_agent_deck::daemon_client::{ClientError, DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::START_ERR_ORCHESTRATION_ID_REQUIRED;
use spec::spec;

mod common;

async fn start_role(
    client: &DaemonClient,
    cwd: &str,
    pane_id: &str,
    role_name: &str,
    orchestration_id: Option<&str>,
) -> Result<String, ClientError> {
    client
        .start_agent(StartAgentOptions {
            command: Some("cat".to_string()),
            cwd: Some(cwd.to_string()),
            display_name: Some(role_name.to_string()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: "review".to_string(),
                role_index: 0,
                role_name: role_name.to_string(),
                is_start_role: true,
                orchestration_cwd: Some(cwd.to_string()),
                display_title: None,
                orchestration_id: orchestration_id.map(str::to_string),
            }),
            ..StartAgentOptions::default()
        })
        .await
}

fn pane_is_registered(daemon: &common::InProcDaemon, pane_id: &str) -> bool {
    daemon
        .registry
        .agent_records()
        .iter()
        .any(|r| r.pane_id_env.as_deref() == Some(pane_id))
}

/// Scenario: a client older than v0.35.0 starts an orchestration role without a
/// per-tab `orchestration_id`. The daemon refuses it with the
/// `orchestration-id-required` error naming the version cut-off, spawns nothing
/// and registers no role for the pane — including for a membership with no role
/// name. The same start carrying a token starts normally and registers its role.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/identity/011")]
async fn identity_011_an_orchestration_start_without_an_orchestration_id_is_refused() {
    let daemon = common::spawn_inprocess_daemon().await;
    let dir = common::race_safe_tempdir();
    let cwd = dir.path().to_string_lossy().into_owned();
    let client = DaemonClient::new(daemon.attach_path.clone());

    // A role with a role name, and one without: both shapes are token-less
    // orchestration memberships, and neither is served.
    for (pane_id, role_name) in [
        ("legacy-orchestrator", "orchestrator"),
        ("legacy-unnamed", ""),
    ] {
        match start_role(&client, &cwd, pane_id, role_name, None).await {
            Err(ClientError::Server(message)) => {
                assert!(
                    message.starts_with(START_ERR_ORCHESTRATION_ID_REQUIRED),
                    "the refusal must carry the stable `{START_ERR_ORCHESTRATION_ID_REQUIRED}` \
                     prefix, got: {message}"
                );
                assert!(
                    message.contains("v0.35.0"),
                    "the refusal must say which clients are no longer supported, got: {message}"
                );
                assert!(
                    message.contains("Nothing was started"),
                    "the refusal must say nothing started, got: {message}"
                );
            }
            other => panic!(
                "a token-less orchestration start (role `{role_name}`) must be REFUSED by the \
                 daemon (issue #463), got: {other:?}"
            ),
        }
        assert!(
            !pane_is_registered(&daemon, pane_id),
            "a refused start must spawn nothing for `{pane_id}`"
        );
    }
    {
        let state = daemon.state.read().await;
        assert!(
            state.pane_orchestration_map.is_empty() && state.pane_role_map.is_empty(),
            "a refused start must register no role: orchestration map {:?}, role map {:?}",
            state.pane_orchestration_map,
            state.pane_role_map
        );
    }

    // Control: the same start with its per-tab token is served, so the refusal
    // is about the missing token and nothing else.
    start_role(
        &client,
        &cwd,
        "tab-orchestrator",
        "orchestrator",
        Some("tab-a"),
    )
    .await
    .expect("an orchestration start carrying its orchestration_id must start");
    assert!(pane_is_registered(&daemon, "tab-orchestrator"));
    let state = daemon.state.read().await;
    let identity = state
        .pane_orchestration_map
        .get("tab-orchestrator")
        .expect("the tokened start registers its role");
    assert_eq!(identity.id, "tab-a");
    assert_eq!(identity.name, "review");
}
