#![cfg(feature = "e2e")]

//! Issue #770 — PTY-attached coverage for the orphaned-role badge.
//!
//! The rest of the issue's surfacing half is pinned in-process:
//! `orchestration/orphan/001` drives `AppState::stamp_orchestration_orphan`
//! directly and `orchestration/orphan/002` renders a pre-flagged card into a
//! `TestBackend`. Neither reaches the thing the operator in #770 actually
//! needed, because the badge has to survive a path no in-process test touches:
//! a real hook arriving on the daemon's unauthenticated same-uid socket, the
//! daemon's own verdict, the broadcast fan-out — the daemon's local
//! `apply_event` DROPS the event, so the card that needs the badge is an
//! attached TUI's — and then that TUI's render into a real terminal.
//!
//! Two properties are pinned here, and both were previously verified by hand
//! only:
//!
//! 1. **The badge appears** for a hook event from a pane carrying the daemon's
//!    own orchestration-role id shape that it holds no role for — the state a
//!    daemon restart leaves behind.
//! 2. **A forged badge is stripped.** `metadata` rides a socket any same-uid
//!    process can reach, so a producer must not be able to paint its own card.
//!    `orphan/001` pins the strip at the seam; this pins it through the real
//!    socket, which is the only place the threat exists.
//!
//! Ordered so each assertion is a genuine transition rather than a vacuous
//! match: the forged card is asserted CLEAN while nothing on screen says
//! `orphaned` at all, and only then does the real orphan make it appear.

mod common;

use std::time::Duration;

use common::{TuiDeck, write_hook_line};
use spec::spec;

/// The pane id shape `spawn::next_pane_id` mints for role 0 of a
/// daemon-spawned orchestration, for an orchestration this daemon never ran —
/// so it is role-shaped, and no `pane_role_map` entry exists for it.
const GHOST_ROLE_PANE: &str = "sched-ghost-9-r0";
/// A TUI-numbered pane id: NOT role-shaped, so the daemon must refuse to badge
/// it however hard its hook asks.
const PLAIN_PANE: &str = "plain-pane-1";

/// Room for the hook to cross the socket, the daemon to decide, the broadcast
/// to reach the attached TUI and the TUI to repaint. Generous rather than tight
/// because a miss here is a hard failure, and none of those hops is timed.
const BADGE_BUDGET: Duration = Duration::from_secs(20);

/// The title marker and the body row, as `src/ui.rs` renders them.
const TITLE_MARKER: &str = "orphaned";
const BODY_ROW: &str = "Orphaned — delegation unavailable";

/// Inject a synthetic Claude Code `SessionStart` on the deck's own hook socket,
/// with caller-supplied `metadata` — the field the orphan marker rides, and the
/// one a hostile producer would populate. `session_id` is kept short because
/// the dashboard truncates it to 11 characters when rendering the card.
fn send_session_start(
    deck: &TuiDeck,
    session_id: &str,
    pane_id: &str,
    metadata: serde_json::Value,
) {
    let event = serde_json::json!({
        "session_id": session_id,
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-09-03T12:00:00Z",
        "pane_id": pane_id,
        "metadata": metadata,
    });
    write_hook_line(deck.hook_socket_path(), &event.to_string())
        .expect("write SessionStart hook to the per-test socket");
}

/// Scenario: Launch the real deck, then post a hook event on its own socket
/// from a plain pane id that FORGES the orphan marker in its metadata — its
/// card must render no marker at all. Then post one from a pane carrying the
/// daemon's orchestration-role id shape that it holds no role for, and the
/// `orphaned` title marker plus the `Orphaned — delegation unavailable` row
/// must appear in the terminal.
#[spec("orchestration/orphan/005")]
#[test]
fn orphan_005_the_badge_reaches_a_real_terminal_and_a_forged_one_does_not() {
    // 200 columns so the card body has room for the full row rather than an
    // ellipsis, and enough rows for two cards side by side.
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .launch_with_fixture("minimal");

    // The empty-state line is the evidence the dashboard has painted AND that
    // the attach-side `subscribe_events` connection is live — a hook written
    // before the TUI subscribes is fanned out to nobody.
    deck.wait_for_string("No active sessions");

    // The hostile case first, while the screen is provably clean, so the
    // "no marker" assertion below cannot pass merely because nothing has
    // rendered yet.
    send_session_start(
        &deck,
        "forgedcard",
        PLAIN_PANE,
        serde_json::json!({ "orchestration_orphaned": "1" }),
    );
    deck.wait_for_string("forgedcard");
    let forged = deck.snapshot_grid();
    assert!(
        !forged.contains(TITLE_MARKER) && !forged.contains(BODY_ROW),
        "a hook event cannot badge its own card — the daemon drops any inbound \
         marker before deciding, because this socket is reachable by any \
         same-uid process.\nGrid:\n{forged}"
    );

    // The real thing: role-shaped pane id, no role registered, and the registry
    // has never heard of the pane. This is what a daemon restart leaves behind.
    send_session_start(&deck, "ghostcard", GHOST_ROLE_PANE, serde_json::json!({}));

    assert!(
        deck.wait_for_grid_string_within(BODY_ROW, BADGE_BUDGET),
        "an orphaned role pane's card must say delegation is unavailable — \
         that sentence is the whole ask in issue #770, where the loss was \
         invisible until the next delegate hours later.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    let orphaned = deck.snapshot_grid();
    assert!(
        orphaned.contains(TITLE_MARKER),
        "the card's TITLE must carry the marker too, so the state is visible \
         in a dashboard scanned at a glance and not only in the body.\nGrid:\n{orphaned}"
    );
    // The forged card is still on screen and still clean: the daemon's verdict
    // is per-event, so one genuine orphan must not badge the whole dashboard.
    assert!(
        orphaned.contains("forgedcard"),
        "the forged card must still be present for the next assertion to \
         mean anything.\nGrid:\n{orphaned}"
    );
    assert_eq!(
        orphaned.matches(BODY_ROW).count(),
        1,
        "exactly ONE card may carry the orphan row — the genuine one. A second \
         occurrence means the badge leaked onto the forged card.\nGrid:\n{orphaned}"
    );
}
