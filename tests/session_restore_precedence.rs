//! PRD #89 M2.2 — daemon-vs-snapshot restore precedence (in-process).
//!
//! Auto-restore on startup tries daemon hydration first; if hydration produced
//! any panes the daemon state wins and the disk snapshot is skipped, otherwise
//! the snapshot is applied. The decision is a pure, structural check on
//! `AppState.managed_pane_ids` exposed as `ui::should_apply_snapshot`, so it can
//! be pinned in-process here without the cross-deck PTY hydration primitive an
//! end-to-end L2 would need (see the tester handoff for `session/restore/005`).
//!
//! This is an in-crate integration test (style of `tests/rehydration.rs`): it
//! runs in the fast tier (`cargo test-fast`), not the `e2e` PTY suite.

use dot_agent_deck::state::AppState;
use dot_agent_deck::ui::should_apply_snapshot;
use spec::spec;

/// Scenario: Drive the `should_apply_snapshot` precedence seam directly. With a
/// fresh `AppState` that has no hydrated managed panes (the daemon-empty case),
/// it must return `true` so the disk snapshot is applied. After registering a
/// single hydrated managed pane id (simulating one pane produced by daemon
/// hydration), it must return `false` so the snapshot restore is skipped and the
/// daemon state wins — pinning the M2.2 precedence without double-restoring.
#[spec("session/restore/005")]
#[test]
fn restore_005_daemon_hydration_wins_over_snapshot() {
    // Zero hydrated managed panes → daemon empty → apply the disk snapshot.
    let mut state = AppState::default();
    assert!(
        should_apply_snapshot(&state),
        "PRD #89 M2.2: with no hydrated managed panes the daemon is empty, so the disk \
         snapshot must be applied (should_apply_snapshot == true)"
    );

    // At least one hydrated managed pane id present → daemon owns the workspace
    // → snapshot restore is skipped so panes are not double-restored.
    state.register_pane("agent-hydrated-1".to_string());
    assert!(
        !should_apply_snapshot(&state),
        "PRD #89 M2.2: once daemon hydration registered a managed pane the daemon state \
         wins, so the snapshot must be skipped (should_apply_snapshot == false)"
    );

    // Additional hydrated panes do not flip the decision back.
    state.register_pane("agent-hydrated-2".to_string());
    assert!(
        !should_apply_snapshot(&state),
        "PRD #89 M2.2: multiple hydrated managed panes must still skip the snapshot"
    );
}
