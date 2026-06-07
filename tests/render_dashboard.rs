//! L1 widget / layout snapshot tests for the dashboard renderer.
//!
//! Per PRD #77 Decision 2 these are in-process tests using ratatui's
//! `TestBackend` plus `insta` file snapshots. No subprocess, no PTY.
//! File-layout-mirrors-catalog (Decision 7): catalog IDs of the form
//! `dashboard/*/NNN` land in this file with function names
//! `<sub-area>_<NNN>_<short_suffix>` per Decision 17.

use std::collections::VecDeque;

use dot_agent_deck::event::AgentType;
use dot_agent_deck::state::{ActiveTool, DashboardStats, SessionState, SessionStatus};
use dot_agent_deck::theme::{ColorPalette, Theme, resolve_palette};
use dot_agent_deck::ui::{
    CardDensityKind, render_card_to_buffer, render_config_gen_prompt_to_buffer,
    render_quit_confirm_to_buffer, render_star_prompt_to_buffer, render_stats_bar_to_buffer,
    render_stop_confirm_to_buffer,
};
use ratatui::style::Color;
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

/// Color-aware sibling of [`buffer_to_text`]. `buffer_to_text` captures the
/// symbol layer ONLY, so it cannot detect a foreground/background color
/// change — exactly the kind of regression PRD #13 is about. This helper
/// run-length-encodes each row into `[fg/bg]"text"` segments so the snapshot
/// pins color: a cell painted with a hardcoded `White` shows up as `White`,
/// distinct from a palette-resolved `Black`. The fully-default terminal
/// background (Reset fg + Reset bg + whitespace) is dropped so the snapshots
/// stay focused on the styled glyphs rather than acres of empty margin.
fn buffer_to_color_text(buffer: &ratatui::buffer::Buffer) -> String {
    use std::fmt::Write as _;

    fn flush(line: &mut String, fg: Color, bg: Color, run: &str) {
        if run.trim().is_empty() && fg == Color::Reset && bg == Color::Reset {
            return;
        }
        let _ = write!(line, "[{fg:?}/{bg:?}]{run:?} ");
    }

    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        let mut run = String::new();
        let mut run_fg = buffer[(0, y)].fg;
        let mut run_bg = buffer[(0, y)].bg;
        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            if cell.fg != run_fg || cell.bg != run_bg {
                flush(&mut line, run_fg, run_bg, &run);
                run.clear();
                run_fg = cell.fg;
                run_bg = cell.bg;
            }
            run.push_str(cell.symbol());
        }
        flush(&mut line, run_fg, run_bg, &run);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Render all six PRD #13 overlay/prompt surfaces with `palette` and emit one
/// named, color-aware `insta` snapshot per surface (suffixed `_{theme}`). The
/// two `#[spec]` tests below differ only by the palette + suffix they pass in,
/// so dark-vs-light is a pure palette swap over identical fixtures.
fn snapshot_overlay_surfaces(palette: ColorPalette, theme: &str) {
    // 1. Stats bar — a representative mix so every status segment renders
    //    plus the `mode:` tail (the separator that is hardcoded DarkGray today).
    let stats = DashboardStats {
        active: 6,
        working: 1,
        thinking: 1,
        waiting: 1,
        errors: 1,
        idle: 1,
        compacting: 1,
        total_tools: 42,
    };
    // 140 cells wide so the full bar fits without truncation — at 80 the
    // line clipped at "1 erro", hiding the `idle`/`tools` segments AND the
    // `mode:` separator (the cell migrated off hardcoded DarkGray @ src
    // 6123), leaving surface #1's whole point unpinned.
    let buf = render_stats_bar_to_buffer(&stats, Some("plan"), palette, 140, 1);
    insta::assert_snapshot!(
        format!("theme_contrast__stats_{theme}"),
        buffer_to_color_text(&buf)
    );

    // 2. Quit-confirm overlay — Detach default-selected (index 0).
    let buf = render_quit_confirm_to_buffer(0, palette, 80, 24);
    insta::assert_snapshot!(
        format!("theme_contrast__quit_{theme}"),
        buffer_to_color_text(&buf)
    );

    // 3. Stop-confirm overlay — No default-selected (index 0), 2 agents.
    let buf = render_stop_confirm_to_buffer(0, 2, palette, 80, 24);
    insta::assert_snapshot!(
        format!("theme_contrast__stop_{theme}"),
        buffer_to_color_text(&buf)
    );

    // 4. Star prompt.
    let buf = render_star_prompt_to_buffer(palette, 80, 24);
    insta::assert_snapshot!(
        format!("theme_contrast__star_{theme}"),
        buffer_to_color_text(&buf)
    );

    // 5. Config-generation prompt — Yes default-selected (index 0).
    let buf = render_config_gen_prompt_to_buffer(0, palette, 80, 24);
    insta::assert_snapshot!(
        format!("theme_contrast__config_gen_{theme}"),
        buffer_to_color_text(&buf)
    );

    // 6. "No agent" empty card — a placeholder session (agent_type None)
    //    routed through the public `render_card_to_buffer` seam. Narrow width
    //    keeps it in the stacked (non-wide) branch. `last_activity = now` so
    //    any rendered elapsed reads `0s ago` (mirrors `pane_004`).
    let now = chrono::Utc::now();
    let placeholder = SessionState {
        session_id: String::new(),
        agent_type: AgentType::None,
        cwd: None,
        status: SessionStatus::Idle,
        active_tool: None,
        started_at: now,
        last_activity: now,
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: None,
        first_prompts: Vec::new(),
        pane_id: None,
        agent_id: None,
    };
    let width: u16 = 40;
    let density = CardDensityKind::Normal;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    let height = density.rendered_height(wide);
    let buf = render_card_to_buffer(&placeholder, None, None, density, palette, 0, width, height);
    insta::assert_snapshot!(
        format!("theme_contrast__no_agent_{theme}"),
        buffer_to_color_text(&buf)
    );
}

/// Scenario: Render all six dot-agent-deck overlay/prompt surfaces — the
/// stats bar, the Quit and Stop confirm dialogs, the star prompt, the
/// config-generation prompt, and the "No agent" empty card — with the DARK
/// palette, capturing each into a color-aware buffer snapshot. Pins that on a
/// dark terminal background every neutral-text cell resolves to the dark
/// palette (White / Gray) so a future change can't silently shift these
/// surfaces off-palette.
#[spec("theme/contrast/001")]
#[test]
fn contrast_001_overlays_dark() {
    snapshot_overlay_surfaces(resolve_palette(Theme::Dark), "dark");
}

/// Scenario: Render the same six overlay/prompt surfaces with the LIGHT
/// palette and snapshot each color-aware buffer. On a light terminal
/// background the neutral text must resolve to the light palette (Black /
/// DarkGray); any surface still emitting a hardcoded White / Gray / DarkGray
/// is unreadable on white, and that mismatch is the regression this test
/// guards.
#[spec("theme/contrast/002")]
#[test]
fn contrast_002_overlays_light() {
    snapshot_overlay_surfaces(resolve_palette(Theme::Light), "light");
}
