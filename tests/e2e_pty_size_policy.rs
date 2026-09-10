#![cfg(all(feature = "e2e", unix))]

//! PRD #882 — L2 coverage for the PTY size policy with more than one client
//! attached to the same agent.
//!
//! A PTY has exactly one window size, so every client attached to an agent sees
//! the same grid. The daemon sizes each agent to the **smallest viewport among
//! its attached viewers**, and larger clients pad the remainder. These tests
//! drive the real spawned binary through a PTY and use the deck's own attach
//! socket as the SECOND client, because that is the only way to reach the case
//! the policy exists for — a single-client run can never disagree with itself.
//!
//! Unix-gated with the rest of the L2 tier: the second client attaches over a
//! Unix domain socket.
//!
//! No LLM tokens are spent — the pane runs `sleep`.

mod common;

use common::{TuiDeck, wait_until};
use dot_agent_deck::daemon_client::DaemonClient;
use spec::spec;
use std::time::Duration;

/// How long to let the daemon settle on a new geometry.
///
/// Generous because the path is a real round trip through a Unix socket plus a
/// `TIOCSWINSZ`, and because the deck's own re-request rides its render loop.
const SETTLE: Duration = Duration::from_secs(10);

/// The geometry the daemon currently has applied for the deck's first agent,
/// read the way any client would — off `AgentRecord` via `list_agents`, which
/// PRD #104 plumbed the PTY dims onto.
///
/// Deliberately asks the DAEMON rather than reading the deck's rendered grid:
/// the policy's subject is the agent's PTY size, and inferring it from painted
/// cells would confuse "the agent is 40 columns wide" with "the pane happens to
/// have 40 columns of content in it right now".
fn agent_pty_size(socket: &std::path::Path) -> Option<(u16, u16)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let socket = socket.to_path_buf();
    runtime.block_on(async move {
        let client = DaemonClient::new(socket);
        let agents = client.list_agents().await.ok()?;
        let agent = agents.first()?;
        Some((agent.rows, agent.cols))
    })
}

/// Open a second client against the deck's own daemon and hold it attached to
/// the first agent, declaring `viewport` as the geometry it can draw.
///
/// Returns the connection, which must be KEPT ALIVE by the caller: the viewer
/// constraint is released when the attach ends, so dropping it is what makes
/// the agent grow back. That is the property the second test asserts, and the
/// reason this returns the connection rather than swallowing it.
fn attach_second_client(
    socket: &std::path::Path,
    viewport: (u16, u16),
) -> (
    tokio::runtime::Runtime,
    dot_agent_deck::daemon_client::AttachConnection,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime for the second client");
    let socket = socket.to_path_buf();
    let conn = runtime.block_on(async move {
        let client = DaemonClient::new(socket);
        let agents = client.list_agents().await.expect("list agents");
        let agent = agents
            .first()
            .expect("the deck spawned an agent to attach to");
        client
            .attach_as_viewer(&agent.id, Some(viewport))
            .await
            .expect("second client attaches as a viewer")
    });
    (runtime, conn)
}

/// Scenario: Launch the deck at 120x32 with a `sleep` pane, then attach a
/// SECOND client to the same agent over the deck's attach socket declaring a
/// deliberately small 20x40 viewport. The daemon sizes the agent to the
/// smallest viewport among its viewers, so the deck's pane — which is far
/// wider — must letterbox: the agent's grid stops short of the pane's box and
/// the remaining columns render blank, rather than the deck's own geometry
/// winning and leaving the small client showing a cropped corner.
#[spec("resize/policy/001")]
#[test]
fn policy_001_a_smaller_second_client_shrinks_the_agent_for_everyone() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 32)
        .with_continue_session("rp", "sleep 600")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");

    let socket = deck.attach_socket_path().to_path_buf();
    let before = agent_pty_size(&socket).expect("the deck spawned an agent");
    assert!(
        before.1 > 40,
        "test prerequisite: the deck's own pane must be WIDER than the viewport the \
         second client is about to declare, or the policy has nothing to arbitrate \
         (deck agent is {before:?})"
    );

    // Hold the small viewer open for the assertion below — releasing it is what
    // gives the constraint back, which is the next test's subject.
    let (_runtime, _conn) = attach_second_client(&socket, (20, 40));

    assert!(
        wait_until(SETTLE, || {
            agent_pty_size(&socket).is_some_and(|(rows, cols)| rows == 20 && cols == 40)
        }),
        "the agent must be sized to the smaller client's viewport, not the deck's; \
         daemon reports {:?}",
        agent_pty_size(&socket)
    );
}

/// Scenario: With the deck and a small second client both attached to the same
/// agent (so the agent is pinned to the small client's 20x40), close the second
/// client's attach. Its constraint is released, and the agent must grow back to
/// the deck's own pane geometry within a frame — without anyone resizing a
/// terminal by hand. This is the half that makes the policy reversible rather
/// than a one-way ratchet down to the smallest pane that ever looked at it.
#[spec("resize/policy/002")]
#[test]
fn policy_002_releasing_the_small_client_grows_the_agent_back() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 32)
        .with_continue_session("rp", "sleep 600")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");

    let socket = deck.attach_socket_path().to_path_buf();
    let (runtime, conn) = attach_second_client(&socket, (20, 40));
    assert!(
        wait_until(SETTLE, || {
            agent_pty_size(&socket).is_some_and(|(_, cols)| cols == 40)
        }),
        "test prerequisite: the agent must first be pinned to the small client"
    );

    // Drop the second client's attach. The daemon prunes its viewer entry and
    // re-applies the minimum over whoever is left, which is the deck alone.
    drop(conn);
    drop(runtime);

    assert!(
        wait_until(SETTLE, || {
            agent_pty_size(&socket).is_some_and(|(_, cols)| cols > 40)
        }),
        "releasing the small client must let the agent grow back; daemon reports {:?}",
        agent_pty_size(&socket)
    );
}
