//! L1 widget / layout snapshot tests for the dashboard renderer.
//!
//! Per PRD #77 Decision 2 these are in-process tests using ratatui's
//! `TestBackend` plus `insta` file snapshots. No subprocess, no PTY.
//! File-layout-mirrors-catalog (Decision 7): catalog IDs of the form
//! `dashboard/*/NNN` land in this file with function names
//! `<sub-area>_<NNN>_<short_suffix>` per Decision 17.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use dot_agent_deck::event::{AgentEvent, AgentType, EventType};
use dot_agent_deck::features::{self, Features};
use dot_agent_deck::state::{ActiveTool, DashboardStats, SessionState, SessionStatus};
use dot_agent_deck::tab::Tab;
use dot_agent_deck::terminal_widget::TerminalWidget;
use dot_agent_deck::ui::{
    CardDensityKind, render_card_to_buffer, render_config_gen_prompt_to_buffer,
    render_dashboard_cards_to_buffer, render_quit_confirm_to_buffer, render_star_prompt_to_buffer,
    render_stats_bar_to_buffer, render_stop_confirm_to_buffer, sync_and_derive_selection,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Widget;
use spec::spec;

/// Width (in terminal cells) at which `render_session_card` flips
/// into its "wide" branch — the rendered card gains an inline
/// `Last: … Tools: …` stats row instead of a stacked one, and the
/// inner card height grows by one row. The constant in the lib's
/// renderer is `let wide = w >= 60;`; keep this mirror in sync
/// (changing it on the lib side without updating here will produce
/// a stale snapshot the next time the test runs).
const RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH: u16 = 60;

/// Serializes any test in this binary that mutates the *process-global*
/// `Features` (only `dashboard/pane/007` today) so a concurrent flip can't
/// bleed across another test's render/assert window under plain `cargo test`
/// (CI's model — one process, tests on threads). `cargo test-fast`/nextest
/// isolates each test in its own process, where this is belt-and-suspenders.
/// Mirrors `FLAG_LOCK` in `tests/experimental_flag.rs`.
static FLAG_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard: snapshots the process-global `Features` on construction and
/// restores it on drop, so a test that flips the experimental flag cannot leak
/// the mutated state to sibling tests in the same binary — even if an assertion
/// panics between the flip and the end of the test. Hold it alongside the
/// `FLAG_LOCK` guard for the full flip→render→assert window.
struct FlagRestore(Features);
impl FlagRestore {
    fn new() -> Self {
        Self(features::current())
    }
}
impl Drop for FlagRestore {
    fn drop(&mut self) {
        features::set_for_test(self.0);
    }
}

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
/// background shows through. (b) Selection is cued the PRD #155 Option-A way —
/// a `▸ ` title prefix plus a `Color::Magenta` + `Modifier::BOLD` border — NOT
/// an absolute background tint, and Magenta is the dedicated `selected` accent
/// role so it never collides with a status color (green/blue/yellow/red) or the
/// `focused` cyan: the selected card's border style must differ from the
/// unselected card's and be magenta-bold. A regression that filled any surface
/// with an absolute background, or that reverted the selection border to a
/// status/focus color, would fail one of these assertions.
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

    // (b) PRD #155 Option A: selection is signalled by a `▸ ` title prefix and a
    //     Magenta+BOLD border — a terminal-relative cue, NOT an absolute
    //     background. Read the left-border cell (`│`, at a mid-height row) of
    //     each card: the selected one must DIFFER from the unselected one and
    //     be Color::Magenta + Modifier::BOLD. Magenta is the dedicated
    //     `selected` accent role; it deliberately does not reuse the working
    //     status green or the focused-pane cyan. (The unselected placeholder
    //     border is a dimmed terminal foreground.)
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
        Color::Magenta,
        "selected card border must use the `selected` role (Color::Magenta), not a status/focus color (Option-A selection cue)"
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

/// Scenario: Render a single dashboard card for a live `AgentType::Pi` session
/// with NO display name (so the card title falls back to the
/// `<agent-type> · <session-id>` form) into a `TestBackend` buffer, then assert
/// the rendered card surface shows the Pi agent-type identity ("Pi") in its
/// title — proving a plain `pi` pane (`command = "pi"`) is rendered as a
/// first-class agent, not "No agent". The fixture's cwd basename and session id
/// carry no capital-`Pi`, so the only way "Pi ·" reaches the grid is via the
/// agent-type Display; the card must NOT show ClaudeCode / OpenCode / No agent.
#[spec("dashboard/pane/007")]
#[test]
fn pane_007_pi_card_shows_pi_identity() {
    // PRD #201 M2.2 (test-plan row 2): a Pi pane's card renders the Pi identity.
    // The `AgentType` Display impl prints "Pi" and `render_session_card`'s
    // no-display-name branch titles the card `<agent_type> · <id>`. The cwd
    // basename (`workspace`) and session id (`orch-01`) deliberately contain no
    // capital `Pi`, so the assertion pins the agent-type identity specifically
    // rather than an incidental substring.
    //
    // PRD #201 M5.1: the Pi first-class identity is gated behind
    // `features::show_pi_agent()` at the render seam (CLAUDE.md #9), so this
    // test forces the experimental flag ON as a precondition; the OFF (hidden)
    // path is `features/gating/004`. This mutates the process-global `Features`,
    // so under plain `cargo test` (CI's shared-process/threaded model) the flip
    // must not leak into sibling tests: serialize with `FLAG_LOCK` and snapshot
    // + restore the prior flag via `FlagRestore` (restored on drop, even if an
    // assertion below panics).
    //
    // `last_activity = now` keeps any rendered `Last: Xs ago` at `0s ago`
    // (mirrors `pane_004`).
    let _flag_lock = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _flag_restore = FlagRestore::new();
    features::set_for_test(Features::test_with(true));
    let now = chrono::Utc::now();
    let session = SessionState {
        session_id: "orch-01".to_string(),
        agent_type: AgentType::Pi,
        cwd: Some("/home/dev/workspace".to_string()),
        status: SessionStatus::Thinking,
        active_tool: None,
        started_at: now,
        last_activity: now,
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: Some("orchestrate the release".to_string()),
        first_prompts: vec!["orchestrate the release".to_string()],
        pane_id: Some("pi-pane-1".to_string()),
        agent_id: Some("1".to_string()),
        // No friendly name → the title uses the `<agent_type> · <id>` form,
        // which is where the Pi identity surfaces.
        display_name: None,
    };
    let width: u16 = 80;
    let density = CardDensityKind::Normal;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    let height = density.rendered_height(wide);
    let buffer = render_card_to_buffer(
        &session,
        None, // no display name
        Some(1),
        density,
        0,     // animation tick
        false, // not selected
        width,
        height,
    );
    let text = buffer_to_text(&buffer);

    // The Pi agent-type identity surfaces in the card title (`Pi · orch-01`).
    assert!(
        text.contains("Pi · orch-01"),
        "a Pi pane's card title must show the Pi agent-type identity \
         (`Pi · orch-01`):\n{text}"
    );
    // ...and it must be Pi specifically — not another agent type or the
    // placeholder.
    for other in ["ClaudeCode", "OpenCode", "No agent"] {
        assert!(
            !text.contains(other),
            "a Pi pane's card must not show `{other}`:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// PRD #155 — centralized color palette (Option A). Border encodes STATUS in
// BOTH deck cards and embedded panes; the dedicated `selected` (Magenta) and
// `focused` (Cyan) accent roles never reuse a status color. These tests read
// the resolved border color out of the rendered buffer (the observable
// end-state) so they survive the palette module's exact API — except
// `theme/guard/003`, which is a deliberate source lint.
// ---------------------------------------------------------------------------

/// Build a live (non-placeholder) session fixture carrying `status`. Used to
/// drive the deck-card border through `render_card_to_buffer` for every status
/// role. `last_activity = now` keeps any rendered elapsed at `0s ago` (mirrors
/// `pane_004`); these tests inspect only the border color, never the body.
fn palette_session(status: SessionStatus) -> SessionState {
    let now = chrono::Utc::now();
    SessionState {
        session_id: "sess-palette".to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/home/dev/example-project".to_string()),
        status,
        active_tool: None,
        started_at: now,
        last_activity: now,
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: Some("do the thing".to_string()),
        first_prompts: vec!["do the thing".to_string()],
        pane_id: Some("pane-1".to_string()),
        agent_id: Some("1".to_string()),
        display_name: None,
    }
}

/// Read the `(fg, modifier)` of a card/pane's left border at a mid-height row.
/// The `Borders::ALL` block paints the left edge (`│`, x=0) with the resolved
/// border style for every inner row, so a mid-height cell is the border color
/// regardless of title/content layout (same seam as `guard_001`).
fn border_style_at_mid(buffer: &ratatui::buffer::Buffer) -> (Color, Modifier) {
    let y = buffer.area().height / 2;
    let cell = &buffer[(0, y)];
    (cell.fg, cell.modifier)
}

/// Render a single deck card for a live agent in `status` (not selected, not
/// focused) and return its resolved border `(fg, modifier)`. 80 cells wide ⇒
/// the wide layout; height comes from the density tier so geometry tracks the
/// production layout module.
fn card_border_at_mid(status: SessionStatus) -> (Color, Modifier) {
    let session = palette_session(status);
    let width: u16 = 80;
    let density = CardDensityKind::Normal;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    let height = density.rendered_height(wide);
    let buffer = render_card_to_buffer(
        &session,
        Some("example-agent"),
        Some(1),
        density,
        0,     // animation tick
        false, // not selected
        width,
        height,
    );
    border_style_at_mid(&buffer)
}

/// Render an embedded pane (`TerminalWidget`) and return its resolved border
/// `(fg, modifier)`. The outer 22×6 area gives a 20×4 inner area; the vt100
/// screen is sized to match so the 1:1 render contract holds (no size-mismatch
/// fallback). `status` is both carried in the pane title and threaded into the
/// widget via `TerminalWidget::with_status`, so the border resolves through the
/// centralized palette.
///
/// PRD #155 Option A: when the pane is neither selected nor focused, its border
/// encodes STATUS — the SAME role the deck card uses (`src/terminal_widget.rs`
/// resolves it from the centralized [`palette`]). With `status = None` the
/// widget keeps the legacy focus-only border (focused → Cyan, else dimmed).
fn pane_border_at_mid(status: Option<SessionStatus>, focused: bool) -> (Color, Modifier) {
    let area = Rect {
        x: 0,
        y: 0,
        width: 22,
        height: 6,
    };
    // Inner area is 20×4; size the screen to match (no contract fallback).
    let parser = vt100::Parser::new(4, 20, 0);
    let parser = Arc::new(Mutex::new(parser));
    let title = match &status {
        Some(s) => format!("{s:?}"),
        None => "pane".to_string(),
    };
    let mut widget = TerminalWidget::new(parser, title, focused);
    if let Some(s) = status {
        widget = widget.with_status(s);
    }
    let mut buf = ratatui::buffer::Buffer::empty(area);
    widget.render(area, &mut buf);
    border_style_at_mid(&buf)
}

/// The six status roles in the centralized palette and the named-ANSI color each
/// must resolve to (PRD #155 locked plan): working=Green, thinking=Blue,
/// compacting=Blue (shares the thinking role), waiting=Yellow, error=Red,
/// idle=DarkGray. The single source of truth shared by the deck-card (T1) and
/// embedded-pane (T2) assertions.
fn status_role_colors() -> [(SessionStatus, Color); 6] {
    [
        (SessionStatus::Working, Color::Green),
        (SessionStatus::Thinking, Color::Blue),
        // Compacting is thinking-adjacent and shares the thinking/Blue role
        // (`palette::status_color` maps Compacting -> Blue) rather than
        // introducing a sixth status color — so it must render Blue in both the
        // deck card and the embedded pane, never an accent (Magenta/Cyan).
        (SessionStatus::Compacting, Color::Blue),
        (SessionStatus::WaitingForInput, Color::Yellow),
        (SessionStatus::Error, Color::Red),
        (SessionStatus::Idle, Color::DarkGray),
    ]
}

/// Scenario: Render a deck card for each of the six agent statuses
/// (working/thinking/compacting/waiting/error/idle), none selected or focused,
/// and assert the card's border color is the matching centralized status role —
/// working=Green, thinking=Blue, compacting=Blue (it shares the thinking role),
/// waiting=Yellow, error=Red, idle=DarkGray. Also assert each status border is a
/// status role and never an accent role (Magenta=selected, Cyan=focused), so a
/// status can never collide with selection/focus. This pins PRD #155 Option A:
/// the deck-card border encodes status via the centralized palette roles.
#[spec("theme/palette/001")]
#[test]
fn palette_001_deck_card_border_is_status_role() {
    for (status, role) in status_role_colors() {
        let (fg, _modifier) = card_border_at_mid(status.clone());
        assert_eq!(
            fg, role,
            "deck card border for {status:?} must use the centralized status role color \
             {role:?}, got {fg:?}"
        );
        // A status role must never collide with an accent role: Magenta is
        // `selected`, Cyan is `focused` (PRD #155 criterion #3). This catches a
        // status (notably Compacting) drifting onto an accent color.
        assert_ne!(
            fg,
            Color::Magenta,
            "status {status:?} border must not reuse the `selected` accent (Magenta)"
        );
        assert_ne!(
            fg,
            Color::Cyan,
            "status {status:?} border must not reuse the `focused` accent (Cyan)"
        );
    }
}

/// Scenario: For each of the six agent statuses
/// (working/thinking/compacting/waiting/error/idle), render the deck card AND an
/// embedded pane (neither selected nor focused) and assert the pane's border
/// color is the SAME as the deck card's for that status — and that both equal the
/// palette status role color. This is the consistency criterion: a given state
/// looks identical as a deck card and as an embedded pane, including compacting,
/// which shares the thinking/Blue role.
#[spec("theme/palette/002")]
#[test]
fn palette_002_pane_border_matches_deck_status_color() {
    for (status, role) in status_role_colors() {
        let card_fg = card_border_at_mid(status.clone()).0;
        let pane_fg = pane_border_at_mid(Some(status.clone()), false).0;
        assert_eq!(
            pane_fg, card_fg,
            "embedded pane border for {status:?} must match the deck card's status color \
             (deck={card_fg:?}, pane={pane_fg:?})"
        );
        assert_eq!(
            card_fg, role,
            "the shared status color for {status:?} must be the {role:?} palette role"
        );
    }
}

/// Scenario: Render a SELECTED deck card and assert its border is the dedicated
/// `selected` accent role — Color::Magenta + Modifier::BOLD — plus the `▸ `
/// title marker, and that this color is NOT a status color (≠ green) and NOT
/// the focused accent (≠ cyan). This pins the Option-A rule that selection is
/// conveyed by a non-status accent that never collides with the palette.
#[spec("theme/palette/003")]
#[test]
fn palette_003_selected_card_border_is_magenta_bold_marker() {
    let session = palette_session(SessionStatus::Working);
    let width: u16 = 80;
    let density = CardDensityKind::Normal;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    let height = density.rendered_height(wide);
    let buffer = render_card_to_buffer(
        &session,
        Some("example-agent"),
        Some(1),
        density,
        0,    // animation tick
        true, // SELECTED
        width,
        height,
    );
    let (fg, modifier) = border_style_at_mid(&buffer);
    assert_eq!(
        fg,
        Color::Magenta,
        "selected card border must use the `selected` role (Color::Magenta), got {fg:?}"
    );
    assert!(
        modifier.contains(Modifier::BOLD),
        "selected card border must be BOLD, got {modifier:?}"
    );
    assert_ne!(
        fg,
        Color::Green,
        "the selected accent must not reuse the working-status green"
    );
    assert_ne!(
        fg,
        Color::Cyan,
        "the selected accent must not reuse the focused-pane cyan"
    );
    assert!(
        buffer_to_text(&buffer).contains('▸'),
        "selected card must carry the `▸ ` selection title marker"
    );
}

/// Scenario: Render a FOCUSED embedded pane and assert its border is the
/// dedicated `focused` accent role — Color::Cyan — and that this color is
/// distinct from every status role (green/blue/yellow/red/dark-gray) and from
/// the `selected` accent (magenta). This pins the Option-A split that keeps
/// focus on Cyan while selection moves to Magenta, so status/selection/focus
/// are provably distinct. Then render a pane that is focused AND carries a real
/// `Working` status and assert its border is still Cyan (the focused accent),
/// NOT Green (the Working status color) — locking the precedence invariant that
/// focus OVERRIDES a present status color.
#[spec("theme/palette/004")]
#[test]
fn palette_004_focused_pane_border_is_cyan_distinct() {
    let (fg, _modifier) = pane_border_at_mid(None, true);
    assert_eq!(
        fg,
        Color::Cyan,
        "focused pane border must use the `focused` role (Color::Cyan), got {fg:?}"
    );
    for collide in [
        Color::Green,
        Color::Blue,
        Color::Yellow,
        Color::Red,
        Color::DarkGray,
        Color::Magenta,
    ] {
        assert_ne!(
            fg, collide,
            "the focused accent (cyan) must be distinct from the {collide:?} status/selection role"
        );
    }

    // PRECEDENCE: focus must win over a PRESENT status color. The case above
    // proves `focused -> Cyan` only when status is None; this constructs a pane
    // that is focused=true AND has a real `Working` status (whose own status
    // color is Green) and asserts the border still resolves to the focused
    // accent (Cyan), never the Working/Green status color — locking Option A's
    // "focused overrides status" rule in the unified border precedence.
    let (focused_with_status_fg, _modifier) =
        pane_border_at_mid(Some(SessionStatus::Working), true);
    assert_eq!(
        focused_with_status_fg,
        Color::Cyan,
        "a focused pane with a present `Working` status must still use the `focused` \
         accent (Color::Cyan), got {focused_with_status_fg:?}"
    );
    assert_ne!(
        focused_with_status_fg,
        Color::Green,
        "focus must OVERRIDE a present status: the border must not fall back to the \
         Working-status Green when the pane is focused"
    );
}

/// Extract the source region of the top-level function whose signature contains
/// `signature` — from the signature to the start of the next top-level `fn` item.
/// The boundary is any column-0 `fn` / `pub fn` / `pub(crate) fn` (or any
/// `pub(...) fn`) declaration: requiring column 0 makes restricted-visibility
/// items like `pub(crate) fn` boundaries too (a missed `pub(crate) fn` between a
/// checked function and the next plain/`pub` fn would otherwise over-extend the
/// region), and it skips indented *nested* inner `fn`s so the region ends at this
/// function's sibling rather than terminating early at an inner helper.
/// Brace-counting is avoided on purpose: bodies here carry `format!` strings with
/// `{ }` placeholders that would skew a brace match. Panics with a clear message
/// if the anchor is missing so a rename surfaces as an explicit lint failure
/// rather than a silent skip.
fn fn_region<'a>(src: &'a str, signature: &str) -> &'a str {
    /// True if `s` begins (at column 0) with a top-level fn declaration:
    /// `fn `, `pub fn `, or any `pub(<vis>) fn ` (e.g. `pub(crate) fn `).
    fn is_top_level_fn_start(s: &str) -> bool {
        match s.strip_prefix("pub") {
            Some(after_pub) => {
                // `pub fn ...` (no visibility qualifier) or `pub(<vis>) fn ...`.
                let after_vis = match after_pub.strip_prefix('(') {
                    Some(inner) => match inner.find(')') {
                        Some(close) => &inner[close + 1..],
                        None => return false,
                    },
                    None => after_pub,
                };
                after_vis.starts_with(" fn ")
            }
            None => s.starts_with("fn "),
        }
    }

    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("source lint anchor `{signature}` not found"));
    let rest = &src[start..];
    // The first newline whose following line (column 0) starts a sibling fn ends
    // the region; that newline index is the region's exclusive upper bound.
    let next_fn = rest
        .match_indices('\n')
        .find(|&(i, _)| is_top_level_fn_start(&rest[i + 1..]))
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    &rest[..next_fn]
}

// Unit-guard for `fn_region` itself (not a `#[spec]` catalog entry): the real
// `guard_003` anchors never sit next to a `pub(crate) fn` or contain a nested
// inner fn, so these boundary paths would otherwise go unexercised. Pins that the
// extracted region (a) reaches content *after* an indented nested inner fn rather
// than terminating early, and (b) stops at a following `pub(crate) fn` sibling
// rather than over-extending past it.
#[test]
fn fn_region_handles_nested_and_restricted_visibility_boundaries() {
    let src = concat!(
        "fn checked() {\n",
        "    fn inner() {\n",
        "        let _ = Color::Magenta;\n", // nested-fn body — must stay INSIDE
        "    }\n",
        "    let _ = Color::Green;\n", // after the nested fn — region must reach it
        "}\n",
        "pub(crate) fn sibling() {\n",
        "    let _ = Color::Yellow;\n", // pub(crate) sibling — must be OUTSIDE
        "}\n",
        "fn last() {}\n",
    );
    let region = fn_region(src, "fn checked");
    assert!(
        region.contains("Color::Magenta"),
        "region must include a nested inner fn's body:\n{region}"
    );
    assert!(
        region.contains("Color::Green"),
        "region must reach content after a nested inner fn (not terminate early):\n{region}"
    );
    assert!(
        !region.contains("Color::Yellow"),
        "region must stop at the following `pub(crate) fn` sibling, not over-extend:\n{region}"
    );
}

/// Scenario: Source-lint the deck-card render path (`src/ui.rs`) and the
/// embedded-pane render path (`src/terminal_widget.rs`): both must reference the
/// centralized `palette`, the deck-card status mapping (`status_style`) and
/// border resolver (`render_session_card`) must carry no inline status/accent
/// `Color::Green/Blue/Yellow/Red/Cyan` literals, the pane path must carry no
/// inline status `Color::Green/Blue/Yellow/Red` literal, and the stats bar
/// (`render_stats_bar`) must carry no inline status `Color::Green/Blue/Yellow/Red`
/// literal — its non-status `Cyan` (active-count) and `LightMagenta` (mode-label)
/// accents stay legal. This is the M4 tightening: the palette is the single
/// source of truth for every status color across all render paths.
#[spec("theme/guard/003")]
#[test]
fn guard_003_render_paths_use_palette_roles() {
    let ui_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui.rs");
    let pane_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/terminal_widget.rs");
    let ui = std::fs::read_to_string(ui_path).expect("src/ui.rs should be readable");
    let pane =
        std::fs::read_to_string(pane_path).expect("src/terminal_widget.rs should be readable");

    // (1) Both render paths reference the centralized palette (single source of
    //     truth), rather than carrying their own inline literals.
    assert!(
        ui.contains("palette"),
        "src/ui.rs must reference the centralized `palette` in its render paths"
    );
    assert!(
        pane.contains("palette"),
        "src/terminal_widget.rs (pane render path) must reference the centralized `palette`"
    );

    // (2) The deck-card status->color mapping carries no inline status/accent
    //     literals — every status color comes from the palette.
    let status_style = fn_region(&ui, "fn status_style");
    for lit in [
        "Color::Green",
        "Color::Blue",
        "Color::Yellow",
        "Color::Red",
        "Color::Cyan",
    ] {
        assert!(
            !status_style.contains(lit),
            "status_style still hardcodes inline `{lit}` — status colors must come from the palette"
        );
    }

    // (3) The deck-card border resolver carries no inline accent/status literal
    //     (notably the selection accent, formerly `Color::Cyan`).
    let card = fn_region(&ui, "fn render_session_card");
    for lit in [
        "Color::Cyan",
        "Color::Green",
        "Color::Blue",
        "Color::Yellow",
        "Color::Red",
        "Color::Magenta",
    ] {
        assert!(
            !card.contains(lit),
            "render_session_card still hardcodes inline `{lit}` — the border must resolve through the palette"
        );
    }

    // (4) The embedded-pane render path carries no inline status literal — the
    //     pane border's status colors must come from the palette (the
    //     consistency criterion mirrored from the deck card).
    for lit in ["Color::Green", "Color::Blue", "Color::Yellow", "Color::Red"] {
        assert!(
            !pane.contains(lit),
            "src/terminal_widget.rs still hardcodes inline `{lit}` — pane status colors must come from the palette"
        );
    }

    // (5) The stats bar's per-status segments carry no inline STATUS literal —
    //     every status color (working/thinking/compacting/waiting/error) routes
    //     through `palette::status_color`. Only the four status roles are
    //     checked here: `render_stats_bar` legitimately keeps the non-status
    //     `Color::Cyan` (active-count header) and `Color::LightMagenta`
    //     (mode label) accents, which are NOT status roles and must stay.
    let stats = fn_region(&ui, "fn render_stats_bar");
    for lit in ["Color::Green", "Color::Blue", "Color::Yellow", "Color::Red"] {
        assert!(
            !stats.contains(lit),
            "render_stats_bar still hardcodes inline `{lit}` — status segment colors must come from the palette"
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

/// Build one event-rich session fixture that fills `render_session_card`
/// to capacity at every density tier: three user prompts (Spacious shows 3,
/// Normal/Compact show the latest 1) and three `ToolStart` events
/// (Normal/Spacious show 3, Compact shows 1). With this fixture the rendered
/// card carries its maximum content lines on every tier, so any rows left
/// blank inside the border are reserved-but-unused — exactly what
/// `dashboard/density/004` asserts must not happen.
///
/// `last_activity = now` keeps the inline `Last: Xs ago` text at `0s ago`
/// (mirrors `pane_004`); these tests only inspect blank/non-blank rows, so the
/// elapsed value never affects the assertion.
fn filled_session() -> SessionState {
    let now = chrono::Utc::now();
    let mut events: VecDeque<AgentEvent> = VecDeque::new();
    for prompt in ["first prompt", "second prompt", "third prompt"] {
        events.push_back(AgentEvent {
            session_id: "sess-fill".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::Thinking,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: now,
            user_prompt: Some(prompt.to_string()),
            metadata: HashMap::new(),
            pane_id: None,
            agent_id: None,
        });
    }
    for (name, detail) in [
        ("Read", "src/main.rs"),
        ("Edit", "src/ui.rs"),
        ("Bash", "cargo test"),
    ] {
        events.push_back(AgentEvent {
            session_id: "sess-fill".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: EventType::ToolStart,
            tool_name: Some(name.to_string()),
            tool_detail: Some(detail.to_string()),
            cwd: None,
            timestamp: now,
            user_prompt: None,
            metadata: HashMap::new(),
            pane_id: None,
            agent_id: None,
        });
    }
    SessionState {
        session_id: "sess-fill".to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/home/dev/example-project".to_string()),
        status: SessionStatus::Working,
        active_tool: Some(ActiveTool {
            name: "Bash".to_string(),
            detail: Some("cargo test".to_string()),
        }),
        started_at: now,
        last_activity: now,
        recent_events: events,
        tool_count: 12,
        last_user_prompt: Some("third prompt".to_string()),
        first_prompts: vec!["first prompt".to_string()],
        pane_id: Some("pane-1".to_string()),
        agent_id: Some("1".to_string()),
        display_name: None,
    }
}

/// Count inner card rows that are blank starting from the row directly above
/// the bottom border and scanning upward, stopping at the first row that
/// carries content. Inner columns only (`1..width-1`) so the left/right border
/// `│` glyphs are ignored. The result is the number of reserved-but-empty rows
/// the card holds below its real content — `0` means the reserved card height
/// equals the rendered content height. The mid-card blank separator line
/// (Normal/Spacious) is never counted because the scan stops at the tool line
/// rendered beneath it.
fn trailing_blank_inner_rows(buffer: &ratatui::buffer::Buffer) -> usize {
    let area = buffer.area();
    let (w, h) = (area.width, area.height);
    if h < 3 || w < 3 {
        return 0;
    }
    let mut count = 0usize;
    let mut y = h - 2; // last inner row (the one just above the bottom border)
    loop {
        let row_blank = (1..w - 1).all(|x| buffer[(x, y)].symbol().trim().is_empty());
        if row_blank {
            count += 1;
        } else {
            break;
        }
        if y == 1 {
            break;
        }
        y -= 1;
    }
    count
}

/// Scenario: Render a single fully-populated session card at each density tier
/// (Compact, Normal, Spacious) in a wide 80-column viewport sized to the tier's
/// own `rendered_height`, then assert the card has zero trailing blank rows —
/// the row directly above the bottom border carries rendered content on every
/// tier. This locks in PRD #147's content-derived `card_height`: the reserved
/// height equals the lines `render_session_card` actually emits (wide 5/8/10),
/// so no row is reserved-but-empty. Before #147 the heights were hardcoded
/// larger than the content (Compact 7 rows for 3 content lines, Normal 9 for 6,
/// Spacious 11 for 8), which would trip this assertion as trailing blank rows.
#[spec("dashboard/density/004")]
#[test]
fn density_004_no_trailing_blank_rows() {
    // PRD #147 M2: reserved card height must equal rendered content height, so
    // a card shows no empty rows below its content at any tier. The fixture
    // fills every tier to capacity (3 prompts + 3 tools); we render at the
    // width-80 "wide" branch (inline stats row) at the tier's declared
    // `rendered_height`, then count trailing blank inner rows — which must be 0.
    let session = filled_session();
    let width: u16 = 80;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    for (density, label) in [
        (CardDensityKind::Compact, "Compact"),
        (CardDensityKind::Normal, "Normal"),
        (CardDensityKind::Spacious, "Spacious"),
    ] {
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
        let trailing = trailing_blank_inner_rows(&buffer);
        assert_eq!(
            trailing,
            0,
            "{label} card reserves {trailing} blank row(s) below its content \
             (height={height}); reserved height must match rendered content:\n{}",
            buffer_to_text(&buffer)
        );
    }
}

/// Replicate `choose_density`'s tier selection using the public
/// `rendered_height` seam: pick the largest tier whose stacked card rows fit
/// in `avail`, falling back to Compact. Mirrors `src/ui.rs::choose_density` so
/// the orchestration capacity test tracks the same selection the renderer uses
/// without depending on the private function.
fn pick_density(n_decks: usize, cols: usize, avail: u16, wide: bool) -> CardDensityKind {
    let rows = n_decks.div_ceil(cols.max(1)) as u16;
    for density in [
        CardDensityKind::Spacious,
        CardDensityKind::Normal,
        CardDensityKind::Compact,
    ] {
        if rows * density.rendered_height(wide) <= avail {
            return density;
        }
    }
    CardDensityKind::Compact
}

/// Scenario: Model the single-column orchestration deck card area (the ~34%
/// width column → one grid column) at a typical terminal where ~48 rows are
/// available for cards, and compute the renderer's own capacity
/// (`visible_rows = available / card_height`) for 7 decks; assert all 7 fit
/// without scrolling and that the 7th deck actually renders in the visible
/// slice, while a much larger deck count still engages scrolling. This locks in
/// PRD #147: with the content-derived Compact height of 5, 48 / 5 = 9 rows fit
/// so all 7 decks show. Before #147 the hardcoded Compact height of 7 fit only
/// 48 / 7 = 6 rows, dropping the 7th deck to a scroll — the regression this
/// test guards against.
#[spec("orchestration/layout/001")]
#[test]
fn layout_001_seven_decks_fit_single_column() {
    // PRD #147 M2: on the orchestration tab decks stack in one column and
    // density bottoms out at Compact. The renderer fits
    // `visible_rows = available_for_density / card_height` rows (src/ui.rs
    // ~7861); the rest scroll. We mirror that math through the public
    // `rendered_height` seam, then confirm it with a real card render.

    // Card-area rows available for cards (production: dashboard_area.height - 2,
    // i.e. a ~50-row card column). WIDE = single orchestration column rendered
    // wide enough (inner width >= 60) to show the inline stats row — the branch
    // PRD #147's capacity example is worked through.
    const AVAILABLE: u16 = 48;
    const COLS: usize = 1;
    const WIDE: bool = true;
    const DECKS: usize = 7;

    let density = pick_density(DECKS, COLS, AVAILABLE, WIDE);
    let card_height = density.rendered_height(WIDE);
    let visible_rows = (AVAILABLE / card_height).max(1) as usize;

    // (1) Capacity: all 7 decks must fit in the single column with no scrolling.
    assert!(
        visible_rows >= DECKS,
        "all {DECKS} orchestration decks must fit without scrolling at \
         card-area height {AVAILABLE} (density={density:?}, card_height={card_height}); \
         only {visible_rows} fit"
    );

    // (2) Buffer proof: the orchestration renderer draws only the first
    //     `visible_rows` of the deck rows (src/ui.rs ~7871); render that visible
    //     slice as real cards and assert the 7th deck shows. If the cards were
    //     too tall to all fit, the 7th would be scrolled off and absent.
    let visible = visible_rows.min(DECKS);
    let session = filled_session();
    let names: Vec<String> = (1..=visible).map(|i| format!("deck-{i}")).collect();
    let cards: Vec<(&SessionState, Option<&str>)> =
        names.iter().map(|n| (&session, Some(n.as_str()))).collect();
    // PRD #113: `selected` is `Option<usize>`; this capacity test highlights
    // nothing, so pass `None` (the old out-of-range `usize::MAX` sentinel).
    let buffer = render_dashboard_cards_to_buffer(&cards, None, density, 0, 64);
    let text = buffer_to_text(&buffer);
    assert!(
        text.contains("deck-7"),
        "the 7th orchestration deck must render in the visible card area \
         (rendered {visible} of {DECKS} decks):\n{text}"
    );

    // (3) Over-correction guard: a deck count well past the (now tighter)
    //     capacity must still engage scrolling — the fix right-sizes the cards,
    //     it does not remove scrolling for genuinely too-many decks.
    const MANY: usize = 20;
    let many_density = pick_density(MANY, COLS, AVAILABLE, WIDE);
    let many_visible = (AVAILABLE / many_density.rendered_height(WIDE)).max(1) as usize;
    assert!(
        many_visible < MANY,
        "with {MANY} decks scrolling must still engage (only {many_visible} fit)"
    );
}
