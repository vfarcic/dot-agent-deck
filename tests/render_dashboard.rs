//! L1 widget / layout snapshot tests for the dashboard renderer.
//!
//! Per PRD #77 Decision 2 these are in-process tests using ratatui's
//! `TestBackend` plus `insta` file snapshots. No subprocess, no PTY.
//! File-layout-mirrors-catalog (Decision 7): catalog IDs of the form
//! `dashboard/*/NNN` land in this file with function names
//! `<sub-area>_<NNN>_<short_suffix>` per Decision 17.

use std::collections::VecDeque;

use dot_agent_deck::event::AgentType;
use dot_agent_deck::state::{ActiveTool, SessionState, SessionStatus};
use dot_agent_deck::tab::Tab;
use dot_agent_deck::theme::{ColorPalette, Theme, resolve_palette};
use dot_agent_deck::ui::{
    CardDensityKind, render_card_to_buffer, render_dashboard_cards_to_buffer,
    sync_and_derive_selection,
};
use spec::spec;

/// Width (in terminal cells) at which `render_session_card` flips
/// into its "wide" branch — the rendered card gains an inline
/// `Last: … Tools: …` stats row instead of a stacked one, and the
/// inner card height grows by one row. The constant in the lib's
/// renderer is `let wide = w >= 60;`; keep this mirror in sync
/// (changing it on the lib side without updating here will produce
/// a stale snapshot the next time the test runs).
const RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH: u16 = 60;

/// Stringify the rendered buffer — one line per row, with cells joined
/// into the symbol layer. `insta` then captures this representation, so
/// snapshot diffs read like the rendered card itself rather than as
/// opaque byte streams. Helper extracted only because it's slightly
/// awkward inline; the docs generator skip-lists it so it doesn't
/// appear in the .md Steps section.
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

/// Scenario: Render a single dashboard card for a Working agent
/// session (with a Read tool active and a recent user prompt) into
/// a `ratatui::TestBackend` buffer at 80 columns × Normal-density
/// height, then snapshot the buffer with `insta`. The card title
/// row should carry the card number (1), the display name
/// `example-coder`, and the `● Working` status badge — and the
/// stats line should show the wide layout's inline
/// `Last: … Tools: …` because 80 cells crosses the wide-layout
/// width threshold.
#[spec("dashboard/pane/004")]
#[test]
fn pane_004_card_title_row() {
    // PRD #77 catalog: dashboard/pane/004 — Card title row carries
    // card number, display name, and a status badge. Snapshot a single
    // Working session in Normal density. The session fixture is
    // inlined per M4.1 reviewer S1 (single-use test-data builder
    // doesn't need its own fn — keeping the test body
    // self-contained also reads as cleaner generated `.md` Steps).
    //
    // `last_activity = Utc::now()` so the rendered `Last: Xs ago`
    // line always reads `0s ago`; pinning it to a fixed past
    // instant let calendar drift turn the rendered elapsed into
    // `262h ago` once the date rolled past the pin (M3 fix).
    let now = chrono::Utc::now();
    let session = SessionState {
        session_id: "sess-abc123".to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/home/dev/example-project".to_string()),
        status: SessionStatus::Working,
        active_tool: Some(ActiveTool {
            name: "Read".to_string(),
            detail: Some("src/main.rs".to_string()),
        }),
        started_at: now,
        last_activity: now,
        recent_events: VecDeque::new(),
        tool_count: 7,
        last_user_prompt: Some("fix the login bug".to_string()),
        first_prompts: vec!["fix the login bug".to_string()],
        pane_id: Some("pane-1".to_string()),
        agent_id: Some("1".to_string()),
    };
    // Dark palette pin: PTY-size + color env aren't strictly
    // relevant in-process, but resolving against Dark prevents the
    // ratatui rendering from drifting if the host environment
    // somehow leaks in.
    let palette: ColorPalette = resolve_palette(Theme::Dark);
    // 80-cell-wide buffer triggers the layout's "wide" branch
    // (inline stats row). The height comes from the density tier
    // itself so the snapshot's geometry tracks the production
    // layout module — M2.1 reviewer S3 (no magic numbers).
    let width: u16 = 80;
    let density = CardDensityKind::Normal;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    let height = density.rendered_height(wide);
    let buffer = render_card_to_buffer(
        &session,
        Some("example-coder"),
        Some(1),
        density,
        palette,
        0, // animation tick
        width,
        height,
    );
    insta::assert_snapshot!(buffer_to_text(&buffer));
}

/// Scenario: Build three dashboard session cards and a
/// `Tab::Dashboard` whose `selected_session_id` points at the
/// **second** card (`sess-beta`), then derive the highlighted index
/// via `ui::sync_and_derive_selection` and render the three cards
/// stacked into a `TestBackend` buffer with `insta`. The derived
/// index is 1 (not 0), so the snapshot shows the `▸` selection
/// marker and highlighted border on the second card while the first
/// and third stay unselected — proving the dashboard highlight
/// follows the stable selected session id (PRD #83 M3) rather than
/// defaulting to card 0.
#[spec("dashboard/pane/005")]
#[test]
fn pane_005_highlight_follows_selected_session_id() {
    // PRD #83 catalog: dashboard/pane/005 — the dashboard card
    // highlight must follow the per-tab `selected_session_id` (the M3
    // fix), not snap back to the top card. We render three cards and
    // point the selection at the 2nd, so a regression that ignored the
    // stable id (highlighting card 0) would visibly diff the snapshot.
    //
    // `last_activity = now` keeps every `Last: Xs ago` line at `0s ago`
    // so calendar drift can't churn the snapshot (mirrors pane_004).
    let now = chrono::Utc::now();
    let make = |sid: &str, pane: &str, name: &str, cwd: &str| SessionState {
        session_id: sid.to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some(cwd.to_string()),
        status: SessionStatus::Working,
        active_tool: Some(ActiveTool {
            name: "Read".to_string(),
            detail: Some("src/main.rs".to_string()),
        }),
        started_at: now,
        last_activity: now,
        recent_events: VecDeque::new(),
        tool_count: 3,
        last_user_prompt: Some("do the thing".to_string()),
        first_prompts: vec!["do the thing".to_string()],
        pane_id: Some(pane.to_string()),
        agent_id: Some(name.to_string()),
    };
    let s1 = make("sess-alpha", "pane-1", "1", "/home/dev/alpha");
    let s2 = make("sess-beta", "pane-2", "2", "/home/dev/beta");
    let s3 = make("sess-gamma", "pane-3", "3", "/home/dev/gamma");

    // Render-order filtered list of (session_id, pane_id) pairs — the
    // shape `sync_and_derive_selection` resolves the selection against.
    let filtered: [(&str, Option<&str>); 3] = [
        ("sess-alpha", Some("pane-1")),
        ("sess-beta", Some("pane-2")),
        ("sess-gamma", Some("pane-3")),
    ];

    // Per-tab selection pinned to the 2nd card's session id. With no
    // focused pane, the derived index resolves purely from the stable
    // session id — and must be 1, not 0.
    let mut tab = Tab::Dashboard {
        selected_session_id: Some("sess-beta".to_string()),
    };
    let selected_index =
        sync_and_derive_selection(&mut tab, None, &filtered).expect("dashboard derives an index");
    assert_eq!(
        selected_index, 1,
        "selection must follow the stable session id to the 2nd card"
    );

    let palette: ColorPalette = resolve_palette(Theme::Dark);
    let cards: [(&SessionState, Option<&str>); 3] = [
        (&s1, Some("example-alpha")),
        (&s2, Some("example-beta")),
        (&s3, Some("example-gamma")),
    ];
    let buffer = render_dashboard_cards_to_buffer(
        &cards,
        selected_index,
        CardDensityKind::Normal,
        palette,
        0, // animation tick
        80,
    );
    insta::assert_snapshot!(buffer_to_text(&buffer));
}
