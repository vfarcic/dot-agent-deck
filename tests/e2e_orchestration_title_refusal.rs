#![cfg(feature = "e2e")]

//! L2 end-to-end coverage for issue #555 on the real binary: the orchestration
//! run title is decided by the daemon, and a TUI whose form admitted a title
//! that another client took in the meantime gets its form back with the
//! collision warning.
//!
//! The race is the one the form's own check cannot see. `Ctrl+n` takes ONE
//! `ListAgents` snapshot of live titles when the form opens and suggests the
//! next free `<folder>-orchestrator-N` from it. This test opens the form, then
//! — playing a second attached client — starts an orchestration under exactly
//! that suggested name straight through the deck's attach socket, and only
//! then presses Enter. The form's snapshot still says the name is free; the
//! daemon says it is not.
//!
//! No LLM: the `orch-deck` fixture's roles are `cat`, and so is the rival's.
//! What is under test is the refusal and what the user sees, not an agent.
//! Credential-free, so lane 1. `orchestration/identity/007`/`008` pin the
//! daemon's decision over the socket and `/009` the form restore at L1; this is
//! the only one that drives both through the spawned binary.
//!
//! Gated behind the `e2e` feature so `cargo test-fast` never compiles it.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::{DOT_AGENT_DECK_PANE_ID, TabMembership};
use dot_agent_deck::daemon_protocol::AttachRequest;
use spec::spec;

/// The warning the form renders for a taken name (`NAME_COLLISION_WARNING`).
const COLLISION_WARNING: &str = "already in use by a live orchestration";

/// Scenario: open the new-pane form on the `orch-deck` fixture and select its
/// orchestration, so the Name field suggests `<folder>-orchestrator-1`; a second
/// client then starts an orchestration under that very name through the deck's
/// attach socket. Pressing Enter must NOT open a tab: the daemon refuses, and
/// the form stays on screen showing "already in use by a live orchestration".
/// Typing `-b` onto the name clears the warning, and Enter then opens the tab
/// under the new name.
#[spec("orchestration/identity/010")]
#[test]
fn identity_010_a_title_taken_by_another_client_after_the_form_opened_is_refused() {
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .launch_with_fixture("orch-deck");
    deck.wait_for_string("No active sessions");

    // The deck's picker starts at its process cwd, which the kernel reports
    // with symlinks resolved — so the orchestration cwd the TUI will send is
    // the canonical fixture root, and the rival has to name the same one.
    let cwd = std::fs::canonicalize(deck.workdir()).expect("canonicalize the fixture root");
    let folder = cwd
        .file_name()
        .expect("the fixture root has a basename")
        .to_string_lossy()
        .into_owned();
    let suggested = format!("{folder}-orchestrator-1");

    deck.send_bytes(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_bytes(b" "); // Space -> confirm current dir -> new-pane form
    deck.wait_for_string("No mode");
    deck.send_bytes(b"\x1b[C"); // Right -> [Orch: demo-orch]
    deck.send_bytes(b"\r"); // Mode -> Name
    deck.wait_for_string(&suggested);

    // The second client: a different tab (its own per-tab token), the same
    // orchestration, directory and title. Its form snapshot — like ours — was
    // taken before either started, so nothing client-side can stop it.
    let socket = deck.attach_socket_path().to_path_buf();
    let rival = common::attach_request_on(
        &socket,
        &AttachRequest::StartAgent {
            command: Some("cat".to_string()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            rows: 24,
            cols: 80,
            env: vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "rival-orchestrator".to_string(),
            )],
            display_name: Some("orchestrator".to_string()),
            tab_membership: Some(TabMembership::Orchestration {
                name: "demo-orch".to_string(),
                role_index: 0,
                role_name: "orchestrator".to_string(),
                is_start_role: true,
                orchestration_cwd: Some(cwd.to_string_lossy().into_owned()),
                display_title: Some(suggested.clone()),
                orchestration_id: Some("rival-tab".to_string()),
            }),
            agent_type: None,
            seed: None,
            authoring_kind: None,
        },
    )
    .expect("reach the deck's attach socket");
    assert!(
        rival.ok,
        "precondition: the rival client's start under a free title must succeed, got {:?}",
        rival.error
    );

    deck.send_bytes(b"\r"); // submit — the form's snapshot still says it is free
    assert!(
        deck.wait_for_grid_string_within(COLLISION_WARNING, Duration::from_secs(10)),
        "submitting a name another client took after the form opened must bring the form back \
         with the collision warning — two tabs with indistinguishable labels is issue #555.\n\
         === rendered grid ===\n{}",
        deck.snapshot_grid()
    );
    let refused_grid = deck.snapshot_grid();
    assert!(
        refused_grid.contains(&suggested) && !refused_grid.contains("[Submit]"),
        "the restored form must keep the typed name and drop [Submit].\n\
         === rendered grid ===\n{refused_grid}"
    );
    let ours = |title: &str| {
        common::agent_records_on(&socket).into_iter().any(|r| {
            matches!(
                &r.tab_membership,
                Some(TabMembership::Orchestration { display_title: Some(t), orchestration_id, .. })
                    if t == title && orchestration_id.as_deref() != Some("rival-tab")
            )
        })
    };
    assert!(
        !ours(&suggested),
        "the refused start must have spawned nothing under the taken title"
    );

    // Recovery: the Name field has focus, so typing appends. A free name
    // clears the warning and the same Enter now opens the tab.
    deck.send_bytes(b"-b");
    deck.wait_for_absence(COLLISION_WARNING);
    deck.send_bytes(b"\r");
    deck.wait_for_absence("New Agent");
    let renamed = format!("{suggested}-b");
    assert!(
        common::wait_until(Duration::from_secs(10), || ours(&renamed)),
        "the resubmitted form under a free name never started its orchestration; records = {:?}",
        common::agent_records_on(&socket)
    );
}
