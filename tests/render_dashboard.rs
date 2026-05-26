//! L1 widget / layout snapshot tests for the dashboard renderer.
//!
//! Per PRD #77 Decision 2 these are in-process tests using ratatui's
//! `TestBackend` plus `insta` file snapshots. No subprocess, no PTY.
//! File-layout-mirrors-catalog (Decision 7): catalog IDs of the form
//! `dashboard/*/NNN` land in this file with function names
//! `<sub-area>_<NNN>_<short_suffix>` per Decision 17.

use std::collections::VecDeque;

use chrono::TimeZone;

use dot_agent_deck::event::AgentType;
use dot_agent_deck::state::{ActiveTool, SessionState, SessionStatus};
use dot_agent_deck::theme::{ColorPalette, Theme, resolve_palette};
use dot_agent_deck::ui::{CardDensityKind, render_card_to_buffer};

/// Construct a deterministic `SessionState` for snapshot tests. All
/// time-bearing fields are pinned to a fixed `Utc` instant so the
/// `Last:` elapsed-time line renders the same value across runs.
fn working_session_fixture() -> SessionState {
    let pinned = chrono::Utc
        .with_ymd_and_hms(2026, 5, 26, 12, 0, 0)
        .single()
        .expect("pinned timestamp is valid");
    SessionState {
        session_id: "sess-abc123".to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/home/dev/example-project".to_string()),
        status: SessionStatus::Working,
        active_tool: Some(ActiveTool {
            name: "Read".to_string(),
            detail: Some("src/main.rs".to_string()),
        }),
        started_at: pinned,
        last_activity: pinned,
        recent_events: VecDeque::new(),
        tool_count: 7,
        last_user_prompt: Some("fix the login bug".to_string()),
        first_prompts: vec!["fix the login bug".to_string()],
        pane_id: Some("pane-1".to_string()),
        agent_id: Some("1".to_string()),
    }
}

/// Stringify the rendered buffer — one line per row, with cells joined
/// into the symbol layer. `insta` then captures this representation, so
/// snapshot diffs read like the rendered card itself rather than as
/// opaque byte streams.
fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn pane_004_card_title_row() {
    // PRD #77 catalog: dashboard/pane/004 — Card title row carries
    // card number, display name, and a status badge. Snapshot a single
    // Working session in Normal density.
    //
    // PTY size + color env are not strictly needed in-process (no PTY
    // is involved), but pinning palette resolution at Dark prevents the
    // ratatui rendering from drifting if the host environment somehow
    // reaches it.
    let session = working_session_fixture();
    let palette: ColorPalette = resolve_palette(Theme::Dark);
    let buffer = render_card_to_buffer(
        &session,
        Some("example-coder"),
        Some(1),
        CardDensityKind::Normal,
        palette,
        0, // animation tick
        80,
        9, // Normal density's card height when stacked (wide)
    );
    insta::assert_snapshot!(buffer_to_text(&buffer));
}
