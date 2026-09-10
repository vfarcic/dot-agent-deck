#![cfg(feature = "e2e")]

//! Issue #367 — L2 guard for watch-wrapped persistent mode panes.
//!
//! `watch = true` is the default for `[[modes.panes]]`, so the configured
//! command runs under `dot-agent-deck watch`. That wrapper used to buffer the
//! child's output and print it only after the child exited, which rendered a
//! permanently blank pane for every command that does not exit (`tail -f`,
//! `kubectl logs -f`, a dev server) — no error, no hint. This drives the real
//! binary end to end and asserts the user-visible outcome: the output shows up
//! in the pane while the command is still running.
//!
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch the deck on a fixture whose only mode has one persistent
/// side pane that keeps the default `watch = true` and runs a command which
/// prints once and then sleeps for ten minutes. Open that mode tab through
/// Ctrl+N → directory picker → new-pane form → Submit, and watch the side
/// pane: the printed sentinel must appear even though the command never exits,
/// and the wrapped command line the shell echoed must be cleared away by the
/// watcher's first paint.
#[spec("tabs/mode/006")]
#[test]
fn mode_006_watch_pane_streams_output_before_the_command_exits() {
    let deck = TuiDeck::launch_with_fixture("mode-watch-stream");
    deck.wait_for_string("No active sessions");

    // Ctrl+N → directory picker → current dir → new-pane form → the fixture's
    // single mode → Submit, mirroring the flow the tab-strip L2 tests use.
    deck.send_bytes(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_bytes(b" ");
    deck.wait_for_string("Mode:");
    deck.send_bytes(b"\x1b[C");
    deck.wait_for_string("stream mode");
    let (scol, srow) = deck.wait_for_in_grid("[Submit]");
    deck.click(scol, srow);
    deck.wait_for_string("Dashboard"); // tab strip appears only with >=2 tabs

    // `WATCH_STREAM_SENTINEL` is assembled by `printf` at runtime, so it cannot
    // come from the echoed command line — only from output the watch wrapper
    // actually streamed. `--interval` appears only in that echo, so its absence
    // proves the wrapper's first paint cleared the screen ahead of the output.
    deck.wait_until_grid(
        "watch-wrapped side pane shows output of a command that has not exited",
        |grid| grid.contains("WATCH_STREAM_SENTINEL") && !grid.contains("--interval"),
    );
}
