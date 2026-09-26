//! The daemon is the authority on which orchestration run titles are taken —
//! issue #555.
//!
//! The `Ctrl+n` form refuses a title a live orchestration already holds, but it
//! decides from ONE `ListAgents` snapshot taken when the form opens
//! (`transition_after_dir_pick`). Two attached clients whose forms were open at
//! the same time each see a snapshot without the other's orchestration, both are
//! told the title is free, and both start — two tabs with indistinguishable
//! labels, and a `delegate`/`work-done` routing story told in names the user can
//! no longer tell apart. A snapshot is not an authority, so the check now lives
//! in the daemon's `StartAgent` handler, before the registry insert.
//!
//! These drive the REAL handler over the attach socket, exactly as a TUI or the
//! desktop does, against an in-process daemon. No LLM and no TUI: what is under
//! test is the accept/refuse decision and what it leaves behind; the TUI half of
//! the refusal (the form coming back with the collision warning) is
//! `orchestration/identity/009`.

#![cfg(unix)]

use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, TabMembership};
use dot_agent_deck::daemon_client::{ClientError, DaemonClient, StartAgentOptions};
use dot_agent_deck::daemon_protocol::START_ERR_ORCHESTRATION_TITLE_IN_USE;
use spec::spec;

mod common;

const ORCHESTRATION: &str = "review";

/// One role of one orchestration TAB. `orchestration_id` is the per-tab token
/// the TUI mints once per tab (PRD #140) — two tabs of the same orchestration in
/// the same directory differ only by it.
struct Role<'a> {
    command: &'a str,
    pane_id: &'a str,
    orchestration_id: &'a str,
    role_index: usize,
    role_name: &'a str,
    is_start_role: bool,
    display_title: Option<&'a str>,
    orchestration_cwd: &'a str,
}

async fn start_role(client: &DaemonClient, role: Role<'_>) -> Result<String, ClientError> {
    client
        .start_agent(StartAgentOptions {
            command: Some(role.command.to_string()),
            cwd: Some(role.orchestration_cwd.to_string()),
            display_name: Some(role.role_name.to_string()),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), role.pane_id.to_string())],
            tab_membership: Some(TabMembership::Orchestration {
                name: ORCHESTRATION.to_string(),
                role_index: role.role_index,
                role_name: role.role_name.to_string(),
                is_start_role: role.is_start_role,
                orchestration_cwd: Some(role.orchestration_cwd.to_string()),
                display_title: role.display_title.map(str::to_string),
                orchestration_id: Some(role.orchestration_id.to_string()),
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

fn assert_title_refusal(result: Result<String, ClientError>, title: &str) {
    match result {
        Err(ClientError::Server(message)) => {
            assert!(
                message.starts_with(START_ERR_ORCHESTRATION_TITLE_IN_USE),
                "the refusal must carry the stable `{START_ERR_ORCHESTRATION_TITLE_IN_USE}` prefix \
                 a client keys its form-reopen on, got: {message}"
            );
            assert!(
                message.contains(title),
                "the refusal must name the title it refused, got: {message}"
            );
        }
        other => panic!(
            "a second orchestration tab under the title `{title}` in the same directory must be \
             REFUSED by the daemon — two tabs with indistinguishable labels is issue #555 — got: \
             {other:?}"
        ),
    }
}

/// Scenario: two clients each open an orchestration tab of the same config in
/// the same directory under the same run title — the race the form's one-shot
/// snapshot cannot see. The first tab's roles all start; the second tab's start
/// role is refused with a specific title-in-use error and nothing is spawned for
/// it, while the same second tab under a different title starts normally. Then
/// eight clients start eight tabs under one fresh title at the same instant, and
/// exactly one of them wins.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/identity/007")]
async fn identity_007_a_concurrent_duplicate_run_title_is_refused_by_the_daemon() {
    let daemon = common::spawn_inprocess_daemon().await;
    let dir = common::race_safe_tempdir();
    let cwd = dir.path().to_string_lossy().into_owned();
    let client = DaemonClient::new(daemon.attach_path.clone());
    let title = "proj-orchestrator-1";

    // Client A: its tab's start role claims the title.
    start_role(
        &client,
        Role {
            command: "cat",
            pane_id: "tab-a-orchestrator",
            orchestration_id: "tab-a",
            role_index: 0,
            role_name: "orchestrator",
            is_start_role: true,
            display_title: Some(title),
            orchestration_cwd: &cwd,
        },
    )
    .await
    .expect("the first tab under a free title must start");

    // Client B, whose form snapshot predates A: same title, same directory,
    // different tab. This is the start the daemon must refuse.
    let refused = start_role(
        &client,
        Role {
            command: "cat",
            pane_id: "tab-b-orchestrator",
            orchestration_id: "tab-b",
            role_index: 0,
            role_name: "orchestrator",
            is_start_role: true,
            display_title: Some(title),
            orchestration_cwd: &cwd,
        },
    )
    .await;
    assert_title_refusal(refused, title);
    assert!(
        !pane_is_registered(&daemon, "tab-b-orchestrator"),
        "a refused start must spawn nothing — the refusal is before the registry insert; \
         records = {:?}",
        daemon.registry.agent_records()
    );

    // The load-bearing half of the design: an orchestration tab is N separate
    // `StartAgent` calls, one per role, and role 0 has just registered the
    // title. Scoping by the per-tab token is what stops the tab colliding with
    // itself — without it every tab would be half-built.
    start_role(
        &client,
        Role {
            command: "cat",
            pane_id: "tab-a-coder",
            orchestration_id: "tab-a",
            role_index: 1,
            role_name: "coder",
            is_start_role: false,
            display_title: Some(title),
            orchestration_cwd: &cwd,
        },
    )
    .await
    .expect("the title-holding tab's own later roles must never collide with it");

    // Control: the refused client picks another name, and it starts — so the
    // refusal above is about the TITLE, not about a second tab of the same
    // orchestration in the same directory (which PRD #140 supports).
    start_role(
        &client,
        Role {
            command: "cat",
            pane_id: "tab-b-orchestrator",
            orchestration_id: "tab-b",
            role_index: 0,
            role_name: "orchestrator",
            is_start_role: true,
            display_title: Some("proj-orchestrator-2"),
            orchestration_cwd: &cwd,
        },
    )
    .await
    .expect("the same second tab under a free title must start");

    // The genuinely concurrent case: eight clients start eight tabs under one
    // fresh title at the same instant, so no start has a live pane to be seen by
    // when the others are checked. Exactly one may win. A daemon that counted
    // only live panes as holders — not a start still in flight — admits several.
    let racing_title = "proj-orchestrator-3";
    let pane_ids: Vec<String> = (0..8).map(|n| format!("race-{n}-orchestrator")).collect();
    let tab_ids: Vec<String> = (0..8).map(|n| format!("race-tab-{n}")).collect();
    let starts = pane_ids.iter().zip(&tab_ids).map(|(pane_id, tab_id)| {
        let client = DaemonClient::new(daemon.attach_path.clone());
        let cwd = cwd.clone();
        async move {
            start_role(
                &client,
                Role {
                    command: "cat",
                    pane_id,
                    orchestration_id: tab_id,
                    role_index: 0,
                    role_name: "orchestrator",
                    is_start_role: true,
                    display_title: Some(racing_title),
                    orchestration_cwd: &cwd,
                },
            )
            .await
        }
    });
    let results = futures_util::future::join_all(starts).await;
    let started = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        started, 1,
        "exactly one of eight concurrent starts under one title may win; results = {results:?}"
    );
    for result in results.into_iter().filter(|r| r.is_err()) {
        assert_title_refusal(result, racing_title);
    }
}

/// Scenario: the edges of what counts as "taken". A tab that typed no name runs
/// under its canonical orchestration name, and a second tab typing that name is
/// refused; the same title in a different directory is a different key and
/// starts; and once every pane of the title-holding tab has exited, the title
/// can be claimed again rather than being unclaimable for the rest of the
/// daemon's life.
#[tokio::test(flavor = "multi_thread")]
#[spec("orchestration/identity/008")]
async fn identity_008_a_title_is_keyed_by_resolved_title_and_cwd_and_freed_on_exit() {
    let daemon = common::spawn_inprocess_daemon().await;
    let dir = common::race_safe_tempdir();
    let cwd = dir.path().to_string_lossy().into_owned();
    let other_dir = common::race_safe_tempdir();
    let other_cwd = other_dir.path().to_string_lossy().into_owned();
    let client = DaemonClient::new(daemon.attach_path.clone());

    // No typed name: the tab's title is the canonical orchestration name, the
    // same `unwrap_or(name)` fallback the TUI's tab label and the form's
    // snapshot apply.
    let release = dir.path().join("release-holder");
    let holder_command = format!(
        "sh -c 'while [ ! -e {} ]; do sleep 0.05; done'",
        release.display()
    );
    start_role(
        &client,
        Role {
            command: &holder_command,
            pane_id: "unnamed-orchestrator",
            orchestration_id: "unnamed-tab",
            role_index: 0,
            role_name: "orchestrator",
            is_start_role: true,
            display_title: None,
            orchestration_cwd: &cwd,
        },
    )
    .await
    .expect("an unnamed orchestration must start");

    let refused = start_role(
        &client,
        Role {
            command: "cat",
            pane_id: "typed-orchestrator",
            orchestration_id: "typed-tab",
            role_index: 0,
            role_name: "orchestrator",
            is_start_role: true,
            display_title: Some(ORCHESTRATION),
            orchestration_cwd: &cwd,
        },
    )
    .await;
    assert_title_refusal(refused, ORCHESTRATION);

    // Keyed on the orchestration cwd as well as the title: the same resolved
    // title in another project is not a collision.
    start_role(
        &client,
        Role {
            command: "cat",
            pane_id: "elsewhere-orchestrator",
            orchestration_id: "elsewhere-tab",
            role_index: 0,
            role_name: "orchestrator",
            is_start_role: true,
            display_title: None,
            orchestration_cwd: &other_cwd,
        },
    )
    .await
    .expect("the same title in a different directory is a different key");

    // The holder's only pane exits. Its role-map entry stays — nothing but a
    // pane CLOSE unregisters it — so a check that did not consult liveness
    // would leave the title unclaimable forever.
    std::fs::write(&release, b"").expect("release the holder's stand-in");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while daemon.registry.has_live_pane("unnamed-orchestrator") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "precondition: the holder's stand-in never exited"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    start_role(
        &client,
        Role {
            command: "cat",
            pane_id: "typed-orchestrator",
            orchestration_id: "typed-tab",
            role_index: 0,
            role_name: "orchestrator",
            is_start_role: true,
            display_title: Some(ORCHESTRATION),
            orchestration_cwd: &cwd,
        },
    )
    .await
    .expect("a title whose every pane has exited must be claimable again");
}
