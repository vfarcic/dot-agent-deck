#![cfg(all(feature = "e2e", unix))]

//! PTY-attached coverage for an orchestration role whose command dies at
//! spawn (issue #1965 Part 2).
//!
//! The reported case: the `mixed` orchestration's reviewer (`devbox run
//! pi-sol`) exited at once because Pi's extension failed to load, and the TUI
//! showed a `No agent` card titled `pane-__dead` that disappeared when
//! selected, with no word of which role died or why. The daemon now reports
//! the exited role with its exit code and last output, and the card says so.
//!
//! Lane 1 (`cargo test-e2e`): both roles are stand-ins (`cat`, and a `sh` that
//! prints a sentinel and exits 7), no agent and no credential.

dot_agent_deck_test_harness::link!();

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// The line the dying role prints before it exits, unique to this test.
const SENTINEL: &str = "ended-role-sentinel-1965";

/// Drop every whitespace run, so a needle that straddles a wrap column in a
/// PTY snapshot still matches.
fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The production new-pane flow: `Ctrl+n` → confirm dir → Right selects the
/// `[Orch: demo-orch]` chip → Enter → Enter.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e");
    deck.send_keys(b" ");
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C");
    deck.send_keys(b"\r");
    deck.send_keys(b"\r");
}

/// What the daemon's `ListAgents` reply says about exited roles.
fn ended_roles(deck: &TuiDeck) -> Option<Vec<dot_agent_deck::ended_role::EndedRole>> {
    common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::ListAgents,
    )
    .ok()
    .and_then(|response| response.ended_roles)
}

/// Scenario: Open the two-role `demo-orch` orchestration from the TUI, whose
/// reviewer role's command prints a sentinel line and exits with code 7 at
/// once. The reviewer's card reads `Exited`, says its command exited with code
/// 7 and shows the sentinel under `Last output:`, instead of a `No agent` card
/// titled `pane-__dead`. Pressing the card's number in command mode says
/// `reviewer exited: its command exited (code 7)` and keeps the card.
#[spec("orchestration/ended-role/001")]
#[test]
fn ended_role_001_a_role_that_dies_at_spawn_says_why_on_its_card() {
    let deck = TuiDeck::builder().launch_with_fixture("ended-role-at-spawn");
    deck.wait_for_string("No active agents");
    open_orchestration(&deck);

    let wanted = squeeze("its command exited (code 7)");
    let shown = common::wait_until(Duration::from_secs(30), || {
        let grid = squeeze(&deck.snapshot_grid());
        grid.contains(&wanted) && grid.contains("Exited") && grid.contains(SENTINEL)
    });
    let grid = deck.snapshot_grid();
    assert!(
        shown,
        "the reviewer's card never said it exited with code 7 and showed its last output.\n\
         The daemon's ended roles: {:?}\nFinal grid:\n{grid}",
        ended_roles(&deck)
    );
    assert!(
        squeeze(&grid).contains("reviewer"),
        "the card names the role.\nFinal grid:\n{grid}"
    );
    assert!(
        !grid.contains("pane-__dead"),
        "the card is not titled with a truncated synthetic id.\nFinal grid:\n{grid}"
    );

    // Selecting the card (command mode, then its number) says which role
    // exited and why, and keeps the card rather than removing it as stale.
    deck.send_keys(b"\x04");
    deck.send_keys(b"2");
    let said = squeeze("reviewer exited: its command exited (code 7)");
    let selected = common::wait_until(Duration::from_secs(10), || {
        squeeze(&deck.snapshot_grid()).contains(&said)
    });
    let grid = deck.snapshot_grid();
    assert!(
        selected,
        "selecting the reviewer's card did not say why it exited.\nFinal grid:\n{grid}"
    );
    assert!(
        grid.contains("Exited") && !grid.contains("Removed stale agent"),
        "the reviewer's card was removed instead of kept.\nFinal grid:\n{grid}"
    );
}
