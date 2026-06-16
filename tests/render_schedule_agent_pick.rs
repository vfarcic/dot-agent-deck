//! PRD #170 round 2 — L1 widget test for the schedule pick-agent modal.
//!
//! Option B (per the coder's design assessment): the Scheduled-Tasks manager
//! Add/Edit no longer spawns the authoring agent immediately — it first opens a
//! small `UiMode::ScheduleAgentPick` modal that reuses the agent-command picker
//! component (`AGENT_COMMAND_PRESETS` + `render_modal_button_row`) so the user
//! picks which agent runs the authoring session, defaulting to the resolved
//! authoring command. This pins the modal's RENDER through a public
//! `render_schedule_agent_pick_to_buffer` seam (a `TestBackend` wrapper
//! mirroring `render_new_pane_form_to_buffer` / `render_dir_picker_to_buffer`).
//! No subprocess, no PTY — pure widget coverage (CLAUDE.md rule 4: L1 for
//! pure widget/layout). The click→spawn and selection halves live in L2
//! (`scheduler/manager/002`, `009`, `010`).
//!
//! RED until the coder adds the `render_schedule_agent_pick_to_buffer` seam (and
//! the modal it renders): the function does not exist yet, so this test target
//! fails to COMPILE — that compile error is the RED signal for this item.

use dot_agent_deck::ui::render_schedule_agent_pick_to_buffer;
use spec::spec;

/// Flatten the rendered buffer to one string of cell symbols (rows joined).
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

/// Scenario: Render the schedule pick-agent modal into an 80×24 `TestBackend`
/// buffer, with the resolved authoring command supplied as `mycmd` (production
/// fills this from the configured `default_command`). Assert the modal surfaces
/// the agent-command picker — both the `claude` and `opencode` presets render,
/// visible by default — AND shows the resolved authoring command (`mycmd`) as the
/// default selection, proving the modal defaults to the configured command while
/// offering the presets as alternatives. RED until the modal + its
/// `render_schedule_agent_pick_to_buffer` seam exist (the seam reference fails to
/// compile today).
#[spec("scheduler/manager/008")]
#[test]
fn manager_008_pick_agent_modal_renders_presets_and_default() {
    let buf = buffer_text(&render_schedule_agent_pick_to_buffer("mycmd", 80, 24));

    // The modal defaults to the resolved authoring command (the seam passes
    // `mycmd`; production fills it from `default_command`).
    assert!(
        buf.contains("mycmd"),
        "the pick-agent modal must show the resolved authoring command (`mycmd` via the \
         seam) as its default selection, got:\n{buf}"
    );

    // The agent-command picker offers the known agent presets, visible by default.
    for preset in ["claude", "opencode"] {
        assert!(
            buf.contains(preset),
            "the pick-agent modal must surface the `{preset}` agent preset (visible by \
             default), got:\n{buf}"
        );
    }
}
