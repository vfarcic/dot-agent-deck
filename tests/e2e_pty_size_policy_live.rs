#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! PRD #882 — the credentialed (lane 2) half of the PTY size policy coverage.
//! See the sibling `e2e_pty_size_policy.rs` for the deterministic half.
//!
//! **Why a real agent is worth a test here, when the policy is about geometry.**
//! The deterministic tests prove the daemon applies the smallest viewport and
//! releases it on detach — but they run `sleep`, which draws nothing and so
//! cannot show whether the thing a user actually looks at survives being
//! resized by somebody else's client. A real agent's TUI reacts to the
//! resulting SIGWINCH by redrawing its whole screen at the new geometry, and
//! that redraw is what makes a letterboxed pane look correct instead of looking
//! like the corruption PRD #104 was opened about. A stand-in proves the
//! plumbing; only a live agent proves the user-visible result.

mod common;

use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_client::DaemonClient;
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const PANE_NAME_SUFFIX: &str = "sizepolicy-claude";
/// Uniquely-named so the assertion cannot be satisfied by an echo of the
/// directive that asks for it, nor by anything else on the grid.
const SENTINEL_FILE: &str = "sizepolicy_r7k2q.txt";

/// Scenario: Runtime-skip unless Claude credentials are available, then launch a
/// genuine interactive Claude Haiku pane in a wide 160-column deck. Attach a
/// SECOND client to that same agent over the deck's attach socket declaring a
/// deliberately narrow 24x60 viewport, so the daemon shrinks the agent for
/// everyone. Then ask the live agent to list the files and name the only `.txt`
/// one, and confirm the sentinel filename paints inside the now-narrow pane —
/// which is only reachable if the real agent survived being resized by a client
/// it knows nothing about, redrew at the imposed geometry, and kept working.
#[spec("resize/policy/003")]
#[test]
fn policy_003_a_live_agent_keeps_working_after_another_client_shrinks_it() {
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let cwd = deck.workdir().to_path_buf();
    std::fs::write(cwd.join(SENTINEL_FILE), "size policy sentinel\n")
        .expect("write the sentinel file the agent will be asked to name");

    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canonical) = cwd.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !trust_paths.contains(&canonical) {
            trust_paths.push(canonical);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed Claude onboarding and per-folder trust");

    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(PANE_NAME_SUFFIX.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash Read").as_bytes());
    let (submit_col, submit_row) = deck
        .find_in_grid("[Submit]")
        .expect("new-pane form should render [Submit]");
    deck.click(submit_col, submit_row);

    assert!(
        deck.wait_for_grid_string_within("Claude Code v", Duration::from_secs(45)),
        "the genuine interactive Claude UI must render before the resize; grid:\n{}",
        deck.snapshot_grid()
    );
    deck.wait_for_string("[Command Mode Ctrl+D]");

    // A second client attaches to the SAME agent with a much narrower viewport.
    // Held open for the rest of the test — releasing it would give the
    // constraint back, which is `resize/policy/002`'s subject, not this one.
    let socket = deck.attach_socket_path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the second client");
    let socket_for_attach = socket.clone();
    let _conn = runtime.block_on(async move {
        let client = DaemonClient::new(socket_for_attach);
        let agents = client.list_agents().await.expect("list agents");
        let agent = agents
            .iter()
            .find(|record| {
                record
                    .display_name
                    .as_deref()
                    .is_some_and(|name| name.ends_with(PANE_NAME_SUFFIX))
            })
            .expect("the Claude pane's agent is registered with the daemon");
        client
            .attach_as_viewer(&agent.id, Some((24, 60)))
            .await
            .expect("second client attaches as a narrow viewer")
    });

    // The live agent is now running at a geometry imposed by a client it has no
    // knowledge of. Drive it and require real work to come back.
    //
    // The sentinel must not already be on screen, or the assertion below could
    // be satisfied by the deck's own chrome — a directory listing, a pane title,
    // a path in a form — rather than by the agent doing the work. Cheap, and it
    // is the difference between a test and a coincidence.
    assert!(
        !deck.snapshot_grid().contains(SENTINEL_FILE),
        "test prerequisite: the sentinel must not be visible before the agent is asked \
         for it; grid:\n{}",
        deck.snapshot_grid()
    );

    // Type straight into the pane: `[Command Mode Ctrl+D]` in the footer means
    // keystrokes are reaching the AGENT and Ctrl+D is how you would leave.
    // Sending Ctrl+D first would hand the directive to the deck instead, which
    // is how the first draft of this test failed against a live Haiku.
    deck.send_keys(
        b"List the files in this directory and reply with the name of the only file ending in .txt",
    );
    deck.send_keys(b"\r");

    assert!(
        deck.wait_for_grid_string_within(SENTINEL_FILE, Duration::from_secs(120)),
        "the live agent must keep working after another client shrank its PTY, and paint \
         the sentinel filename in the resized pane; grid:\n{}",
        deck.snapshot_grid()
    );
}
