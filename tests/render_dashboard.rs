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
use dot_agent_deck::tab::Tab;
use dot_agent_deck::ui::{
    CardDensityKind, render_card_to_buffer, render_config_gen_prompt_to_buffer,
    render_dashboard_cards_to_buffer, render_quit_confirm_to_buffer, render_star_prompt_to_buffer,
    render_stats_bar_to_buffer, render_stop_confirm_to_buffer, sync_and_derive_selection,
};
use ratatui::style::{Color, Modifier};
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
        display_name: None,
    };
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
        0,     // animation tick
        false, // not selected
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

/// Render the five PRD #13 overlay/prompt surfaces — stats bar, Quit and Stop
/// confirm dialogs, star prompt, config-generation prompt — into color-aware
/// buffers and return one `(label, buffer)` per surface. Both
/// `theme/contrast/001` and `theme/guard/001` drive these same seams; the
/// label is only used to point assertion failures at the offending surface.
fn overlay_buffers() -> Vec<(&'static str, ratatui::buffer::Buffer)> {
    // Representative mix so every status segment renders, 140 cells wide so the
    // whole bar fits without truncation (mirrors the prior contrast fixtures).
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
    vec![
        (
            "stats",
            render_stats_bar_to_buffer(&stats, Some("plan"), 140, 1),
        ),
        ("quit", render_quit_confirm_to_buffer(0, 80, 24)),
        ("stop", render_stop_confirm_to_buffer(0, 2, 80, 24)),
        ("star", render_star_prompt_to_buffer(80, 24)),
        ("config_gen", render_config_gen_prompt_to_buffer(0, 80, 24)),
    ]
}

/// Build a placeholder ("No agent") session card fixture. `last_activity = now`
/// so any rendered elapsed reads `0s ago` (mirrors `pane_004`).
fn placeholder_card(selected: bool) -> ratatui::buffer::Buffer {
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
        display_name: None,
    };
    let width: u16 = 40;
    let density = CardDensityKind::Normal;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    let height = density.rendered_height(wide);
    render_card_to_buffer(
        &placeholder,
        None,
        None,
        density,
        0,
        selected,
        width,
        height,
    )
}

/// Scenario: Render the five dot-agent-deck overlay/prompt surfaces — the stats
/// bar, the Quit and Stop confirm dialogs, the star prompt, and the
/// config-generation prompt — into color-aware buffers and assert that NO cell
/// carries an absolute `Color::Rgb(..)` foreground or background. Under the
/// PRD #13 terminal-relative model every overlay must paint its canvas with
/// `Color::Reset` (inheriting the terminal's own background) and use only
/// `Reset` / named-ANSI foregrounds, so any `Rgb` token in the color dump is a
/// reference-frame violation. A regression that reintroduced an absolute fill
/// (e.g. `bg(Color::Rgb(..))`) would surface as an `Rgb` token and fail here.
#[spec("theme/contrast/001")]
#[test]
fn contrast_001_overlays_reference_frame() {
    for (label, buf) in overlay_buffers() {
        let dump = buffer_to_color_text(&buf);
        assert!(
            !dump.contains("Rgb"),
            "overlay `{label}` emits an absolute Color::Rgb(..) — \
             terminal-relative surfaces must use Reset/ANSI only:\n{dump}"
        );
    }
}

/// Scenario: Render the five overlay seams plus a session card in both the
/// unselected and SELECTED states, then assert two terminal-relative
/// properties. (a) NO rendered cell across any surface has a `Color::Rgb(..)`
/// background — backgrounds must be `Color::Reset` so the terminal's own
/// background shows through. (b) Selection is cued the PRD #13 Option-A way —
/// a `▸ ` title prefix plus a `Color::Cyan` + `Modifier::BOLD` border — NOT an
/// absolute background tint: the selected card's border style must differ from
/// the unselected card's and be cyan-bold. A regression that filled any surface
/// with an absolute background, or that dropped the cyan-bold selection border,
/// would fail one of these assertions.
#[spec("theme/guard/001")]
#[test]
fn guard_001_no_absolute_backgrounds() {
    // (a) No Color::Rgb(..) BACKGROUND on any cheaply-seamable surface. The
    //     color dump renders each cell as `[fg/bg]"text"`, so an Rgb background
    //     shows up as the `/Rgb(` token (Rgb in the bg position after the `/`).
    let unselected = placeholder_card(false);
    let selected = placeholder_card(true);
    let mut surfaces = overlay_buffers();
    surfaces.push(("card", unselected.clone()));
    surfaces.push(("card_selected", selected.clone()));
    for (label, buf) in &surfaces {
        let dump = buffer_to_color_text(buf);
        assert!(
            !dump.contains("/Rgb"),
            "surface `{label}` paints an absolute Color::Rgb(..) background — \
             backgrounds must be Color::Reset:\n{dump}"
        );
    }

    // (b) PRD #13 Option A: selection is signalled by a `▸ ` title prefix and a
    //     Cyan+BOLD border — a terminal-relative cue, NOT an absolute
    //     background. Read the left-border cell (`│`, at a mid-height row) of
    //     each card: the selected one must DIFFER from the unselected one and
    //     be Color::Cyan + Modifier::BOLD. (The unselected placeholder border is
    //     a dimmed terminal foreground.)
    let border_style = |buf: &ratatui::buffer::Buffer| {
        let y = buf.area().height / 2;
        let cell = &buf[(0, y)];
        (cell.fg, cell.modifier)
    };
    let (unsel_fg, unsel_mod) = border_style(&unselected);
    let (sel_fg, sel_mod) = border_style(&selected);
    assert_ne!(
        (sel_fg, sel_mod),
        (unsel_fg, unsel_mod),
        "selected card border must differ from the unselected card border"
    );
    assert_eq!(
        sel_fg,
        Color::Cyan,
        "selected card border must use Color::Cyan (Option-A selection cue)"
    );
    assert!(
        sel_mod.contains(Modifier::BOLD),
        "selected card border must be BOLD (Option-A selection cue)"
    );

    // The `▸ ` selection prefix appears only on the selected card's title row.
    assert!(
        buffer_to_text(&selected).contains('▸'),
        "selected card must carry the `▸ ` selection prefix in its title"
    );
    assert!(
        !buffer_to_text(&unselected).contains('▸'),
        "unselected card must NOT carry the `▸ ` selection prefix"
    );
}

/// Scenario: Read `src/ui.rs` from disk and assert it contains none of the
/// forbidden absolute-background patterns — `bg(Color::Rgb`,
/// `bg(palette.terminal_bg)`, `bg(palette.selected_bg)`, `bg(palette.tab_bar_bg)`.
/// This source lint guards the `render_frame` canvas/tab-bar fills, which paint
/// the whole window and aren't cheaply reachable through a render seam. Under
/// the PRD #13 terminal-relative model none of these absolute fills may remain.
/// Reintroducing any of the four patterns in `src/ui.rs` would fail this lint.
#[spec("theme/guard/002")]
#[test]
fn guard_002_no_absolute_bg_in_source() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui.rs");
    let src = std::fs::read_to_string(path).expect("src/ui.rs should be readable");
    for forbidden in [
        "bg(Color::Rgb",
        "bg(palette.terminal_bg)",
        "bg(palette.selected_bg)",
        "bg(palette.tab_bar_bg)",
    ] {
        assert!(
            !src.contains(forbidden),
            "src/ui.rs still contains forbidden absolute-background pattern `{forbidden}` — \
             terminal-relative surfaces must use Color::Reset / Modifier::REVERSED"
        );
    }
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
        display_name: None,
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

    let cards: [(&SessionState, Option<&str>); 3] = [
        (&s1, Some("example-alpha")),
        (&s2, Some("example-beta")),
        (&s3, Some("example-gamma")),
    ];
    let buffer = render_dashboard_cards_to_buffer(
        &cards,
        // PRD #113: the renderer now takes an active/inactive `Option<usize>`;
        // `Some(idx)` paints the highlight on that card.
        Some(selected_index),
        CardDensityKind::Normal,
        0, // animation tick
        80,
    );
    insta::assert_snapshot!(buffer_to_text(&buffer));
}
