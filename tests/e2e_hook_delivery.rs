#![cfg(feature = "e2e")]

//! L2 end-to-end hook-delivery tests. Each function spawns the real
//! `dot-agent-deck` binary inside an isolated PTY, writes a hook
//! payload to the per-test hook socket, and asserts on the rendered
//! grid through a `vt100` parser. PRD #77 Decision 2 + Decision 6.
//!
//! Decision 6: this file is gated behind the `e2e` feature so CI
//! (which runs only `cargo test-fast`) never compiles it.

mod common;

use common::{TuiDeck, write_hook_line};
use dot_agent_deck::hook_ingest::{MAX_HOOK_CONNECTIONS, MAX_HOOK_LINE_BYTES};
use spec::spec;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn hook_peer_closed(stream: &mut UnixStream) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set hook peer read timeout");
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            true
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            false
        }
        Err(error) => panic!("unexpected error while waiting for hook peer closure: {error}"),
    }
}

fn write_line_on_open_hook(stream: &mut UnixStream, event: &serde_json::Value) {
    let line = event.to_string();
    stream
        .write_all(line.as_bytes())
        .expect("write JSON on an open hook connection");
    stream
        .write_all(b"\n")
        .expect("terminate JSON on an open hook connection");
    stream
        .flush()
        .expect("flush JSON on an open hook connection");
}

/// Scenario: Launch the deck against the `minimal` fixture, wait
/// for the empty dashboard to render, then write a synthetic
/// Claude Code `SessionStart` hook payload (with `pane_id =
/// pane-m2-001`, `session_id = m2demo`, `agent_type = claude_code`)
/// directly to the per-test hook socket. The deck's daemon auto-
/// registers the unknown pane on its first `SessionStart` event,
/// so a card titled `m2demo` should appear on the dashboard within
/// the test budget. No real LLM tokens are spent — the harness
/// injects the event in-process.
#[spec("hooks/delivery/001")]
#[test]
fn delivery_001_session_start_creates_card() {
    // PRD #77 catalog: hooks/delivery/001 — A Claude Code SessionStart
    // hook arriving at the daemon's hook socket creates a session entry
    // on the dashboard. The harness redirects `DOT_AGENT_DECK_SOCKET`
    // to a per-test path so the deck-spawned daemon binds there;
    // `write_hook_line` then injects the JSON payload that the daemon
    // already accepts on the hook socket (see `run_hook_loop` in
    // `src/daemon.rs`).
    let deck = TuiDeck::launch_with_fixture("minimal");

    // Wait for the deck to finish painting its initial dashboard so the
    // attach-side `subscribe_events` connection is live before we inject
    // — otherwise a fast write can land before the TUI subscribes. The
    // empty-state line is sufficient evidence the dashboard rendered;
    // wait_until_quiescent would race the TUI's periodic redraw tick.
    deck.wait_for_string("No active sessions");

    // The hook event uses a session_id short enough to render in full
    // (the dashboard truncates to 11 chars), and a fresh pane_id that
    // the deck has not seen — `apply_event`'s SessionStart auto-register
    // branch will adopt it and a card will appear.
    let event = serde_json::json!({
        "session_id": "m2demo",
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-05-26T12:00:00Z",
        "pane_id": "pane-m2-001",
    });

    // `pane-m2-001` is a pane no daemon spawned, so this is the foreign-agent
    // path and carries no capability token (issue #318).
    write_hook_line(deck.hook_socket_path(), &event.to_string(), None)
        .expect("write SessionStart hook to per-test socket");

    // Asserting via `wait_for_string` against the rendered grid — the
    // catalog explicitly says "loose substring match on the session_id
    // or display_name".
    deck.wait_for_string("m2demo");
}

/// Scenario: Launch the real deck, send more than the production hook-line byte limit without a newline, and observe from the peer that the daemon closes that connection while the empty dashboard stays unchanged. A valid SessionStart on a fresh connection must still create its card afterward.
#[spec("hooks/delivery/008")]
#[test]
fn delivery_008_oversized_newline_free_hook_is_closed() {
    const RECOVERY_SESSION: &str = "boundok";

    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let mut oversized = UnixStream::connect(deck.hook_socket_path())
        .expect("connect oversized hook peer to the per-test socket");
    oversized
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set oversized hook write timeout");
    let payload = vec![b'x'; MAX_HOOK_LINE_BYTES + 1];
    let closed_while_writing = match oversized.write_all(&payload) {
        Ok(()) => {
            oversized.flush().expect("flush oversized hook payload");
            false
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            true
        }
        Err(error) => panic!("unexpected error while writing oversized hook payload: {error}"),
    };

    deck.wait_until_grid_then_hold(
        "oversized newline-free payload creates no card",
        Duration::from_millis(300),
        |grid| grid.contains("No active sessions"),
    );
    let oversized_was_closed = closed_while_writing || hook_peer_closed(&mut oversized);

    let recovery_event = serde_json::json!({
        "session_id": RECOVERY_SESSION,
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-08-24T12:00:00Z",
        "pane_id": "post-bound-foreign-pane",
    });
    write_hook_line(deck.hook_socket_path(), &recovery_event.to_string(), None)
        .expect("write well-formed hook on a fresh connection after oversized peer");
    deck.wait_until_grid("fresh hook connection still creates its card", |grid| {
        grid.contains(RECOVERY_SESSION)
            && grid
                .lines()
                .any(|line| line.contains("ClaudeCode") && line.contains("Idle"))
    });

    assert!(
        oversized_was_closed,
        "the daemon kept the newline-free payload connection open after {} bytes, exceeding MAX_HOOK_LINE_BYTES={MAX_HOOK_LINE_BYTES}; final grid:\n{}",
        payload.len(),
        deck.snapshot_grid()
    );
}

/// Scenario: Launch the real deck, fill every production hook-connection slot with idle peers, and prove the last admitted peer can register a card before opening one excess connection. The excess peer must close promptly, while a valid ToolStart sent through an already-admitted peer still moves that card to Working and leaves the deck alive.
#[spec("hooks/delivery/009")]
#[test]
fn delivery_009_excess_hook_connection_is_closed() {
    const SESSION_ID: &str = "capcard";
    // Same assertion the tester wrote, moved into a `const` block: now that
    // `MAX_HOOK_CONNECTIONS` is a real constant, clippy's
    // `assertions_on_constants` refuses the runtime form under the repo's
    // `-D warnings` gate. The check is unchanged, only earlier.
    const {
        assert!(
            MAX_HOOK_CONNECTIONS > 0,
            "the production hook connection cap must admit at least one peer"
        )
    };

    let deck = TuiDeck::launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    let mut admitted: Vec<UnixStream> = (0..MAX_HOOK_CONNECTIONS)
        .map(|index| {
            UnixStream::connect(deck.hook_socket_path()).unwrap_or_else(|error| {
                panic!("connect admitted hook peer {index}/{MAX_HOOK_CONNECTIONS}: {error}")
            })
        })
        .collect();

    let session_start = serde_json::json!({
        "session_id": SESSION_ID,
        "agent_type": "claude_code",
        "event_type": "session_start",
        "timestamp": "2026-08-24T12:00:00Z",
        "pane_id": "connection-cap-foreign-pane",
    });
    write_line_on_open_hook(
        admitted
            .last_mut()
            .expect("positive hook cap has a last admitted peer"),
        &session_start,
    );
    deck.wait_until_grid("last admitted hook peer registers its card", |grid| {
        grid.contains(SESSION_ID)
            && grid
                .lines()
                .any(|line| line.contains("ClaudeCode") && line.contains("Idle"))
    });

    let mut excess = UnixStream::connect(deck.hook_socket_path())
        .expect("connect one hook peer beyond MAX_HOOK_CONNECTIONS");
    let excess_was_closed = hook_peer_closed(&mut excess);

    let tool_start = serde_json::json!({
        "session_id": SESSION_ID,
        "agent_type": "claude_code",
        "event_type": "tool_start",
        "tool_name": "Read",
        "tool_detail": "src/daemon.rs",
        "timestamp": "2026-08-24T12:00:01Z",
        "pane_id": "connection-cap-foreign-pane",
    });
    write_line_on_open_hook(
        admitted
            .first_mut()
            .expect("positive hook cap has a first admitted peer"),
        &tool_start,
    );
    deck.wait_until_grid(
        "an admitted hook peer still drives its card after excess refusal",
        |grid| {
            grid.contains(SESSION_ID)
                && grid
                    .lines()
                    .any(|line| line.contains("ClaudeCode") && line.contains("Working"))
        },
    );

    assert!(
        excess_was_closed,
        "the daemon kept a hook connection open beyond MAX_HOOK_CONNECTIONS={MAX_HOOK_CONNECTIONS}; final grid:\n{}",
        deck.snapshot_grid()
    );
}
