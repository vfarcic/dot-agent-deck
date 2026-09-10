#![cfg(feature = "e2e")]

//! L2 coverage for the hook socket's ingest bounds (issues #903 / #319).
//!
//! The bounds themselves are pinned in-process against the real
//! `run_hook_loop` by `hooks/ingest/001`–`003` (`src/daemon.rs`), which is
//! where the boundary arithmetic and the connection accounting can actually be
//! asserted. What this file adds is the half those cannot reach: the bound
//! holding inside the daemon the **spawned binary** lazily starts, with a real
//! TUI attached to it, so a refusal cannot take the deck down or leave the
//! dashboard stale.
//!
//! Lane 1 — no real agent is involved anywhere, so this runs in CI on every PR.

mod common;

use common::{TuiDeck, write_hook_line};
use spec::spec;
use std::time::Duration;

/// How long to give a card that must NOT appear. It only has to outlast the
/// path a card genuinely takes: `hooks/delivery/001` has one on screen well
/// inside a second, and the ordinary event asserted first in this test has
/// already made that whole round trip before the window opens.
const ABSENCE_WINDOW: Duration = Duration::from_secs(5);

/// One `session_start` line for `session_id`, padded with ASCII filler in its
/// metadata so the serialized line reaches exactly `target_len` bytes. The
/// filler is plain ASCII, so JSON encoding grows it 1:1 and the difference
/// between the unpadded line and the target IS the padding.
fn session_start_line(session_id: &str, target_len: usize) -> String {
    let build = |padding: usize| {
        serde_json::json!({
            "session_id": session_id,
            "agent_type": "claude_code",
            "event_type": "session_start",
            "timestamp": "2026-09-08T12:00:00Z",
            "pane_id": format!("pane-{session_id}"),
            "metadata": { "padding": "x".repeat(padding) },
        })
        .to_string()
    };
    let padding = target_len
        .checked_sub(build(0).len())
        .expect("the target line length must exceed the envelope");
    let line = build(padding);
    assert_eq!(
        line.len(),
        target_len,
        "padding arithmetic must land exactly"
    );
    line
}

/// Scenario: Launch the real deck against the `minimal` fixture and wait for
/// the empty dashboard. Write a `session_start` one byte past
/// `MAX_HOOK_LINE_BYTES` to the per-test hook socket, then an ordinary
/// `session_start` on a fresh connection. The ordinary card must appear — the
/// deck survived the refusal and its daemon is still serving — and the
/// over-long event's card must never appear, because the daemon declined the
/// message whole instead of truncating it into a partial event.
#[spec("hooks/ingest/004")]
#[test]
fn ingest_004_over_long_hook_line_is_refused_by_the_spawned_daemon() {
    let deck = TuiDeck::launch_with_fixture("minimal");

    // The attach-side event subscription is live once the dashboard has
    // painted, so nothing written below can land before the TUI is listening
    // (the same precondition `hooks/delivery/001` establishes).
    deck.wait_for_string("No active sessions");

    // One byte over. The write itself may legitimately fail partway — the
    // daemon stops reading and drops the connection the moment the line
    // crosses the cap, which is the behaviour under test — so its outcome is
    // deliberately not asserted on.
    let over_cap = session_start_line(
        "overcapl2",
        dot_agent_deck::bounded_read::MAX_HOOK_LINE_BYTES + 1,
    );
    let _ = write_hook_line(deck.hook_socket_path(), &over_cap);

    // An ordinary event on a fresh connection, written AFTER the refusal. Its
    // card appearing is two facts at once: the deck did not die, and its
    // daemon is still accepting and applying hook events.
    let ordinary = serde_json::json!({
        "session_id": "okafterl2",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-09-08T12:00:01Z",
        "pane_id": "pane-okafterl2",
    });
    write_hook_line(deck.hook_socket_path(), &ordinary.to_string())
        .expect("an ordinary hook line must still be accepted after a refusal");
    deck.wait_for_string("okafterl2");

    // Only now is absence meaningful: the later event has completed the whole
    // socket → daemon → broadcast → render round trip, so the over-long one is
    // not merely still in flight. Before the fix `next_line()` buffered it
    // whole and this card rendered like any other.
    assert!(
        !deck.wait_for_grid_string_within("overcapl2", ABSENCE_WINDOW),
        "an over-long hook line must never reach a card; the daemon refuses the \
         message rather than truncating it, so no partially-populated event is \
         applied. Grid:\n{}",
        deck.snapshot_grid()
    );
}
