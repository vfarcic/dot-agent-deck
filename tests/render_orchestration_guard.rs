//! L1 coverage for the same-cwd orchestration warning in the new-pane form.
//!
//! The test drives the production form renderer through a narrow public
//! `TestBackend` seam. The seam accepts the form cwd plus the orchestration
//! cwds learned from live daemon records so both the collision and fresh-cwd
//! cases exercise the same warning decision used by the interactive flow.

use dot_agent_deck::ui::render_new_pane_orchestration_guard_to_buffer;
use spec::spec;

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Scenario: Render the new-pane form with an orchestration selected, first
/// for a cwd absent from the daemon's live orchestration records and then for
/// a matching cwd. Only the collision warns that `.dot-agent-deck`
/// coordination files and the working tree are shared and points the user at
/// a worktree, without blocking the form.
#[spec("orchestration/guard/001")]
#[test]
fn guard_001_warns_for_same_cwd_live_orchestration_only() {
    let fresh = buffer_text(&render_new_pane_orchestration_guard_to_buffer(
        "/work/fresh",
        &["/work/already-live"],
        100,
        28,
    ));
    assert!(
        !fresh.contains(".dot-agent-deck") && !fresh.to_lowercase().contains("worktree"),
        "a fresh cwd must not show the shared-resource warning, got:\n{fresh}"
    );

    let collision = buffer_text(&render_new_pane_orchestration_guard_to_buffer(
        "/work/already-live",
        &["/work/already-live"],
        100,
        28,
    ));
    assert!(
        collision.contains(".dot-agent-deck"),
        "same-cwd orchestration warning must name the shared .dot-agent-deck coordination files, got:\n{collision}"
    );
    assert!(
        collision.to_lowercase().contains("worktree"),
        "same-cwd orchestration warning must point the user at a worktree, got:\n{collision}"
    );
    assert!(
        collision.contains("[Submit]"),
        "the warning is non-blocking: the form must retain its Submit action, got:\n{collision}"
    );
}
