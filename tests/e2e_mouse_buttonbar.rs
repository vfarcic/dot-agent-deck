#![cfg(feature = "e2e")]

//! PRD #80 M2 — L2 synthetic test for the global button bar.
//!
//! Spawns the real `dot-agent-deck` binary inside an isolated PTY, finds
//! the `New Pane` button in the persistent bottom button bar, and clicks
//! it via an SGR mouse report. The click must produce the SAME outcome as
//! pressing Ctrl+N — the directory picker (`Select Directory`) opens —
//! proving click and keyboard funnel into the one shared `Action`.
//! Decision 6: gated behind the `e2e` feature so `cargo test-fast` never
//! compiles it.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// Scenario: Launch the deck against the `minimal` fixture, wait for the
/// empty dashboard, locate the `[New Pane Ctrl+N]` button in the bottom
/// button bar, and left-click it. The same directory picker that Ctrl+N
/// opens (titled `Select Directory`) must appear — demonstrating
/// click→action parity through the shared dispatch funnel.
#[spec("mouse/buttonbar/003")]
#[test]
fn buttonbar_003_click_new_pane_opens_picker() {
    // PRD #127: render at a roomy full-screen width so the bottom bar shows the
    // FULL `[New Pane Ctrl+N]` label. At the default 120 cols the bar correctly
    // collapses to shortcut-only chips once the always-shown Scheduled Tasks
    // button is included (~133 cells > 120), so the labeled-button lookup would
    // miss. 200 cols fits the full labeled set (mirrors L1 buttonbar_005).
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .launch_with_fixture("minimal");

    // Empty dashboard rendered → the bottom button bar is on screen.
    deck.wait_for_string("No active sessions");

    // Find the New Pane button by its on-screen label and click inside it.
    let (col, row) = deck.wait_for_in_grid("[New Pane");
    deck.click(col + 1, row);

    // Ctrl+N's outcome: the directory picker opens. Same action, via click.
    deck.wait_for_string("Select Directory");
}

/// Scenario: Seed a global `schedules.toml` (one enabled task, `btnopen`) via
/// `DOT_AGENT_DECK_SCHEDULES`, launch against the `minimal` fixture, wait for
/// the empty dashboard, locate the `[Scheduled Tasks …]` button in the bottom
/// button bar, and left-click it. The "Scheduled Tasks" manager dialog must
/// open — demonstrating click→action parity for the dialog open-shortcut
/// (PRD #80), just like the `[New Pane Ctrl+N]` button. We confirm the dialog
/// opened by waiting for the seeded task name `btnopen`, which renders only
/// inside the dialog's list (not in the button-bar label). RED today: there is
/// NO Scheduled Tasks button in the bar (the open-shortcut bypasses the action
/// registry entirely), so the lookup fails.
#[spec("mouse/buttonbar/004")]
#[test]
fn buttonbar_004_click_scheduled_tasks_opens_manager() {
    // PRD #127 finding #4 (RED). Pinned for the coder: the new bar button's
    // label must START WITH `[Scheduled` (e.g. `[Scheduled Tasks s]`, mirroring
    // the inline-shortcut convention of `[New Pane Ctrl+N]` / `[Help ?]`), so
    // this black-box lookup finds it.
    let dir = common::harness_tempdir().expect("scratch tempdir");
    let sched_path = dir.path().join("schedules.toml");
    std::fs::write(
        &sched_path,
        "[[scheduled_tasks]]\n\
         name = \"btnopen\"\n\
         cron = \"0 9 * * *\"\n\
         working_dir = \"/tmp\"\n\
         command = \"cat\"\n\
         prompt = \"btnopen prompt\"\n\
         enabled = true\n",
    )
    .expect("write fixture schedules.toml");

    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .launch_with_fixture("minimal");

    // Empty dashboard rendered → the bottom button bar is on screen.
    deck.wait_for_string("No active sessions");

    // Find the Scheduled Tasks button by its on-screen label and click inside it.
    let (col, row) = deck.wait_for_in_grid("[Scheduled");
    deck.click(col + 1, row);

    // Same outcome as the keyboard open-shortcut: the manager dialog opens,
    // listing the seeded task. `btnopen` is unique to the dialog list.
    deck.wait_for_string("btnopen");
    drop(dir);
}

/// Scenario: Launch one live card, enter Help so the production bar renders `[Close Ctrl+W]` disabled, click that dimmed button, then click Help's own `[Close]` control. Help must close normally with no close-confirm modal and the daemon agent must remain alive, proving the disabled bar button was not hit-testable.
#[spec("mouse/buttonbar/007")]
#[test]
fn buttonbar_007_dimmed_close_is_inert_outside_command_mode() {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 40)
        .with_continue_session("dimmed-close-target", "cat")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    deck.send_bytes(b"\x04");
    deck.wait_for_string("[Back to Pane Ctrl+D]");
    deck.send_bytes(b"?");
    deck.wait_for_string("Create new pane");

    // Both lookups poll (`wait_for_in_grid`) rather than reading the grid once:
    // each follows an input event, and a single-shot read landing mid-repaint
    // sees a transiently cleared region. The `[Close]` lookup below was
    // observed failing exactly that way under tier load while passing alone.
    let (close_col, close_row) = deck.wait_for_in_grid("[Close Ctrl+W]");
    deck.click(close_col + 1, close_row);

    // The help overlay's exact `[Close]` label is distinct from the bar's
    // `[Close Ctrl+W]`. If the dimmed click wrongly armed close confirmation,
    // this second real click either hits that modal or leaves it visible; both
    // outcomes fail the assertions below.
    let (help_close_col, help_close_row) = deck.wait_for_in_grid("[Close]");
    deck.click(help_close_col + 1, help_close_row);
    deck.wait_for_absence("Create new pane");
    deck.wait_for_string("[Back to Pane Ctrl+D]");

    let grid = deck.snapshot_grid();
    assert!(!grid.contains("Close selected pane?"), "{grid}");
    assert!(
        !grid.contains("Close this tab and all its panes?"),
        "{grid}"
    );
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "dimmed-close-target",
            true,
            Duration::from_secs(1),
        ),
        "clicking the dimmed Close button outside command mode must not stop the agent"
    );
}
