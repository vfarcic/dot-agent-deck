//! PRD #170 (unify schedule creation) — L1 widget test for the new-pane form
//! MODE-LOCKED to schedule authoring.
//!
//! The Scheduled-Tasks manager's Add/Edit no longer opens a bespoke
//! `ScheduleAgentPick` modal — it reuses the SAME `Ctrl+n` directory-picker →
//! new-pane form, locked to the built-in `schedule` authoring option. In that
//! locked mode the form drops the Mode cycler and the Name field, leaving only
//! **Dir** (the picked directory) + **Command** (free-text, pre-filled from the
//! configured `default_command`), and retitles the modal ` New Schedule ` (Add)
//! / ` Edit Schedule ` (Edit). This pins that locked RENDER through a public
//! `render_new_pane_form_schedule_to_buffer` seam (a `TestBackend` wrapper
//! mirroring `render_new_pane_form_to_buffer`). No subprocess, no PTY — pure
//! widget coverage (CLAUDE.md rule 4: L1 for pure widget/layout). The
//! spawn-on-confirm halves live in L2 (`scheduler/form/002`, `scheduler/form/003`).
//!
//! RED until the coder adds the `render_new_pane_form_schedule_to_buffer` seam
//! and the `NewPaneFormState::new_schedule_locked` constructor + locked render
//! branches it drives: the function does not exist yet, so this test target
//! fails to COMPILE — that compile error is the RED signal for this item.

use dot_agent_deck::ui::render_new_pane_form_schedule_to_buffer;
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

/// Scenario: Render the schedule-locked new-pane form into an 80×24
/// `TestBackend` buffer in BOTH its Add variant (`edit = false`) and its Edit
/// variant (`edit = true`). Assert the locked form shows ONLY the Dir and
/// Command fields plus the `[Submit]`/`[Cancel]` buttons — with NO Mode cycler
/// (`No mode` chip absent) and NO Name field (`Name:` absent) — and that its
/// title reflects the action: ` New Schedule ` for Add, ` Edit Schedule ` for
/// Edit. RED until the coder adds the locked render branches (today the form
/// always shows the Mode cycler + Name field and titles itself ` New Agent `).
#[spec("scheduler/form/001")]
#[test]
fn form_001_schedule_locked_form_shows_only_dir_and_command() {
    // ── Add variant: ` New Schedule ` ─────────────────────────────────────
    let add = buffer_text(&render_new_pane_form_schedule_to_buffer(false, 80, 24));

    assert!(
        add.contains("New Schedule"),
        "the Add (locked) schedule form must title itself ` New Schedule `, got:\n{add}"
    );
    // The two surviving fields render.
    assert!(
        add.contains("Dir:"),
        "the locked schedule form must render the Dir field, got:\n{add}"
    );
    assert!(
        add.contains("Command:"),
        "the locked schedule form must render the (free-text) Command field, got:\n{add}"
    );
    // Submit / Cancel buttons stay (shared form chrome).
    for btn in ["[Submit]", "[Cancel]"] {
        assert!(
            add.contains(btn),
            "the locked schedule form must render the {btn} button, got:\n{add}"
        );
    }
    // The Mode cycler is GONE — its always-present `No mode` chip must not render.
    assert!(
        !add.contains("No mode"),
        "the locked schedule form must HIDE the Mode cycler (no `No mode` chip), got:\n{add}"
    );
    // The Name field is GONE — the schedule's own name is authored conversationally.
    assert!(
        !add.contains("Name:"),
        "the locked schedule form must HIDE the Name field, got:\n{add}"
    );

    // ── Edit variant: ` Edit Schedule ` ───────────────────────────────────
    let edit = buffer_text(&render_new_pane_form_schedule_to_buffer(true, 80, 24));
    assert!(
        edit.contains("Edit Schedule"),
        "the Edit (locked) schedule form must title itself ` Edit Schedule `, got:\n{edit}"
    );
    // Same locked shape on Edit: Dir + Command, no Mode cycler, no Name field.
    assert!(
        edit.contains("Dir:") && edit.contains("Command:"),
        "the Edit schedule form must still render only Dir + Command, got:\n{edit}"
    );
    assert!(
        !edit.contains("No mode") && !edit.contains("Name:"),
        "the Edit schedule form must also hide the Mode cycler and Name field, got:\n{edit}"
    );
}
