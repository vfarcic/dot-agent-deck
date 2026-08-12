//! L1 coverage for the persistent current-mode indicators.
//!
//! These tests exercise the production `TerminalWidget` and bottom-bar render
//! paths in process. The frame-level hardware-cursor test uses a dedicated
//! TestBackend seam because `Frame::set_cursor_position` has no buffer cell to
//! inspect.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::agent_pty::PTY_RESIZE_DIM_MAX;
use dot_agent_deck::embedded_pane::EmbeddedPaneController;
use dot_agent_deck::event::AgentType;
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::state::{SessionState, SessionStatus};
use dot_agent_deck::terminal_widget::TerminalWidget;
use dot_agent_deck::ui::{
    BannerTier, COMMAND_BANNER_TTL, CardDensityKind, CommandBannerSignal, CommandBannerState,
    CommandBannerVisibility, ReconcileSeamPane, UiMode, command_banner_tier,
    observe_command_banner_key_burst, observe_focused_agent_key_scroll,
    observe_focused_agent_mouse_scroll, observe_pane_input_scrollback_reconcile,
    observe_pane_input_without_focused_pane, render_button_bar_for_mode_to_buffer,
    render_button_bar_to_buffer, render_button_bar_with_bindings_to_buffer,
    render_card_for_mode_to_buffer, render_card_to_buffer, render_command_banner_pane_to_buffer,
    render_focused_pane_cursor_for_mode_to_position,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use spec::spec;

const COMMAND_CHIP: &str = " COMMAND ";
const TYPING_CHIP: &str = " TYPING ";

fn buffer_to_text(buffer: &Buffer) -> String {
    let area = buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn buffer_to_styled_text(buffer: &Buffer) -> String {
    fn flush(line: &mut String, style: ratatui::style::Style, run: &str) {
        if run.trim().is_empty() && style == ratatui::style::Style::default() {
            return;
        }
        let _ = write!(line, "[{style:?}]{run:?} ");
    }

    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        let mut run = String::new();
        let mut run_style = buffer[(0, y)].style();
        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            if cell.style() != run_style {
                flush(&mut line, run_style, &run);
                run.clear();
                run_style = cell.style();
            }
            run.push_str(cell.symbol());
        }
        flush(&mut line, run_style, &run);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn nonblank_rows(buffer: &Buffer) -> usize {
    let area = buffer.area();
    (area.y..area.y + area.height)
        .filter(|&y| (area.x..area.x + area.width).any(|x| buffer[(x, y)].symbol().trim() != ""))
        .count()
}

fn selected_card_fixture() -> SessionState {
    // `last_activity` is nudged 30s into the future of `now` (issue #350):
    // `mode_deck_001_selected_card_styles` pins this fixture's rendered card
    // into a color/style-aware snapshot, and the bottom border's `Last:` field
    // is computed by `format_elapsed` (src/ui.rs) from `Utc::now()` at render
    // time, not at fixture-build time. Seeding it to exactly `now` raced that
    // computation across a whole second boundary under any scheduling delay.
    // The forward nudge relies on `format_elapsed`'s existing clamp of a
    // negative delta to zero (`delta.num_seconds().max(0)`), so the rendered
    // value holds at `0s` for 30s. Committed snapshot is unchanged.
    let now = chrono::Utc::now();
    SessionState {
        session_id: "mode-card".to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/work/mode-card".to_string()),
        status: SessionStatus::Working,
        active_tool: None,
        started_at: now,
        last_activity: now + chrono::Duration::seconds(30),
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: None,
        first_prompts: Vec::new(),
        pane_id: Some("pane-mode-card".to_string()),
        agent_id: None,
        display_name: None,
        shell_synthetic_working: false,
    }
}

fn assert_no_rgb(buffer: &Buffer, context: &str) {
    let area = buffer.area();
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            assert!(
                !matches!(cell.fg, Color::Rgb(..)) && !matches!(cell.bg, Color::Rgb(..)),
                "{context} cell ({x}, {y}) must not carry an absolute RGB colour, got {:?}",
                cell.style()
            );
        }
    }
}

fn assert_mode_chip_at_origin(buffer: &Buffer, expected: &str, context: &str) {
    let actual = (0..expected.chars().count() as u16)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>();
    assert_eq!(
        actual, expected,
        "{context} must render the current-mode chip at the left edge"
    );

    let required = Modifier::REVERSED | Modifier::BOLD;
    for x in 0..expected.chars().count() as u16 {
        let cell = &buffer[(x, 0)];
        assert!(
            cell.modifier.contains(required),
            "{context} chip cell {x} must be REVERSED|BOLD, got {:?}",
            cell.style()
        );
        assert!(
            !matches!(cell.fg, Color::Rgb(..)) && !matches!(cell.bg, Color::Rgb(..)),
            "{context} chip cell {x} must not carry an absolute RGB colour, got {:?}",
            cell.style()
        );
    }
}

fn inner_rect(buffer: &Buffer) -> Rect {
    let area = buffer.area();
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn find_ascii_text(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let chars = needle.chars().collect::<Vec<_>>();
    let area = buffer.area();
    if chars.len() > usize::from(area.width) {
        return None;
    }
    for y in area.y..area.y + area.height {
        for x in area.x..=area.x + area.width - chars.len() as u16 {
            if chars
                .iter()
                .enumerate()
                .all(|(offset, ch)| buffer[(x + offset as u16, y)].symbol() == ch.to_string())
            {
                return Some((x, y));
            }
        }
    }
    None
}

fn modifier_bounds(buffer: &Buffer, modifier: Modifier) -> Option<Rect> {
    let area = buffer.area();
    let mut min_x = u16::MAX;
    let mut min_y = u16::MAX;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut found = false;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if buffer[(x, y)].modifier.contains(modifier) {
                found = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    found.then(|| Rect::new(min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
}

fn assert_banner_within_inner_area(buffer: &Buffer, context: &str) -> Option<Rect> {
    let inner = inner_rect(buffer);
    let bounds = modifier_bounds(buffer, Modifier::REVERSED);
    if let Some(bounds) = bounds {
        assert!(
            bounds.x >= inner.x
                && bounds.y >= inner.y
                && bounds.x + bounds.width <= inner.x + inner.width
                && bounds.y + bounds.height <= inner.y + inner.height,
            "{context} banner {bounds:?} must stay within pane inner area {inner:?}"
        );
    }
    bounds
}

fn assert_command_inner_area_is_dimmed(buffer: &Buffer, context: &str) {
    let inner = inner_rect(buffer);
    for y in inner.y..inner.y + inner.height {
        for x in inner.x..inner.x + inner.width {
            let cell = &buffer[(x, y)];
            assert!(
                cell.modifier.contains(Modifier::DIM) || cell.modifier.contains(Modifier::REVERSED),
                "{context} inner cell ({x}, {y}) must remain DIM unless the REVERSED banner overlays it, got {:?}",
                cell.style()
            );
        }
    }
}

fn assert_inner_area_has_no_modifier(buffer: &Buffer, modifier: Modifier, context: &str) {
    let inner = inner_rect(buffer);
    for y in inner.y..inner.y + inner.height {
        for x in inner.x..inner.x + inner.width {
            let cell = &buffer[(x, y)];
            assert!(
                !cell.modifier.contains(modifier),
                "{context} inner cell ({x}, {y}) must not carry {modifier:?}, got {:?}",
                cell.style()
            );
        }
    }
}

fn render_cursor_widget(input_active: bool) -> Buffer {
    const SCREEN_ROWS: u16 = 3;
    const SCREEN_COLS: u16 = 8;
    const CURSOR_ROW: u16 = 1;
    const CURSOR_COL: u16 = 3;

    let mut parser = vt100::Parser::new(SCREEN_ROWS, SCREEN_COLS, 0);
    parser.process(b"\x1b[2;4H");
    assert_eq!(parser.screen().cursor_position(), (CURSOR_ROW, CURSOR_COL));

    let parser = Arc::new(Mutex::new(parser));
    let widget =
        TerminalWidget::new(parser, "pane".to_string(), true).with_input_active(input_active);
    let area = Rect::new(0, 0, SCREEN_COLS + 2, SCREEN_ROWS + 2);
    let mut buffer = Buffer::empty(area);
    widget.render(area, &mut buffer);
    buffer
}

/// Scenario: Render the same focused terminal screen twice with its vt100 cursor parked at a known cell. PaneInput must retain today's exact black-on-LightGreen bold block, while command mode must render no painted cursor styling of any kind.
#[spec("mode/cursor/001")]
#[test]
fn mode_cursor_001_painted_cursor_respects_input_active() {
    let live = render_cursor_widget(true);
    let live_cursor = &live[(4, 2)];
    assert_eq!(
        live_cursor.style(),
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightGreen)
            .underline_color(Color::Reset)
            .add_modifier(Modifier::BOLD),
        "PaneInput must retain the exact painted block-cursor style"
    );

    let command = render_cursor_widget(false);
    let command_cursor = &command[(4, 2)];
    for (x, y) in [(3, 2), (5, 2), (4, 1), (4, 3)] {
        assert_eq!(
            command_cursor.style(),
            command[(x, y)].style(),
            "a focused pane in command mode must style the vt100 cursor cell exactly like neighbouring non-cursor cell ({x}, {y}); cursor={:?}, neighbour={:?}",
            command_cursor.style(),
            command[(x, y)].style()
        );
    }
    assert_eq!(
        command_cursor.modifier,
        Modifier::empty(),
        "a focused pane in command mode must render no painted cursor modifier; got {:?}",
        command_cursor.style()
    );
}

/// Scenario: Render the same focused pane through a complete TestBackend frame in PaneInput and command mode. The live frame must request a terminal cursor position, while the command-mode frame must leave the terminal cursor hidden.
#[spec("mode/cursor/002")]
#[test]
fn mode_cursor_002_terminal_cursor_hidden_in_command_mode() {
    let typing = render_focused_pane_cursor_for_mode_to_position(UiMode::PaneInput);
    let command = render_focused_pane_cursor_for_mode_to_position(UiMode::Normal);

    assert!(
        typing.is_some(),
        "a focused pane in PaneInput must request the terminal emulator cursor"
    );
    assert_eq!(
        command, None,
        "a focused pane in command mode must not call Frame::set_cursor_position"
    );
}

/// Scenario: Render the production bottom bar once in command mode and once in PaneInput. A left-anchored REVERSED|BOLD chip must name the current mode as COMMAND or TYPING without using an absolute RGB colour, and a snapshot pins both complete bars.
#[spec("mode/chip/001")]
#[test]
fn mode_chip_001_bottom_bar_names_current_mode() {
    let config = KeybindingConfig::default();
    let command = render_button_bar_for_mode_to_buffer(&config, UiMode::Normal, 200, 2);
    let typing = render_button_bar_for_mode_to_buffer(&config, UiMode::PaneInput, 200, 2);

    assert_mode_chip_at_origin(&command, COMMAND_CHIP, "command-mode bottom bar");
    assert_mode_chip_at_origin(&typing, TYPING_CHIP, "PaneInput bottom bar");

    insta::assert_snapshot!(
        "mode_chip_001_current_mode_bars",
        format!(
            "COMMAND MODE\n{}\n\nPANE INPUT\n{}",
            buffer_to_text(&command),
            buffer_to_text(&typing)
        )
    );
}

/// Scenario: Render the global-only Mode-tab bar, the context-rich Dashboard/Orchestration bar, and the PaneInput bar. Every context must keep the chip at the same left edge while retaining the destination-naming Ctrl+D button beside it.
#[spec("mode/chip/002")]
#[test]
fn mode_chip_002_is_universal_and_keeps_destination_button() {
    let config = KeybindingConfig::default();
    let dashboard = render_button_bar_for_mode_to_buffer(&config, UiMode::Normal, 200, 2);
    let mode_tab = render_button_bar_to_buffer(200);
    // Dashboard and Orchestration share the production Cards bottom-bar path;
    // render it independently here so failures name both user-visible contexts.
    let orchestration = render_button_bar_with_bindings_to_buffer(&config, 200, 2);

    for (context, buffer) in [
        ("Dashboard", &dashboard),
        ("Mode tab", &mode_tab),
        ("Orchestration tab", &orchestration),
    ] {
        assert_mode_chip_at_origin(buffer, COMMAND_CHIP, context);
        let text = buffer_to_text(buffer);
        assert!(
            text.contains("[Back to Pane Ctrl+D]"),
            "{context} must retain the destination button alongside the COMMAND chip\n{text}"
        );
    }

    let typing = render_button_bar_for_mode_to_buffer(&config, UiMode::PaneInput, 200, 2);
    assert_mode_chip_at_origin(&typing, TYPING_CHIP, "PaneInput on every tab");
    let typing_text = buffer_to_text(&typing);
    assert!(
        typing_text.contains("[Command Mode Ctrl+D]"),
        "PaneInput must retain the destination button alongside the TYPING chip\n{typing_text}"
    );
}

/// Scenario: Sweep every bottom-bar width from zero through 24 columns in command, PaneInput, Filter, and Rename modes, then render the command bar at the known wrapping widths. COMMAND and TYPING must appear symmetrically at the shared 10-column threshold without panics, and the rendered row count must exactly consume the rows reserved at 19, 40, 80, 120, and 200 columns.
#[spec("mode/chip/003")]
#[test]
fn mode_chip_003_narrow_threshold_is_symmetric_and_preserves_row_budget() {
    let config = KeybindingConfig::default();

    for width in 0..=24 {
        let mut rendered = Vec::new();
        for mode in [
            UiMode::Normal,
            UiMode::PaneInput,
            UiMode::Filter,
            UiMode::Rename,
        ] {
            let result = std::panic::catch_unwind(|| {
                render_button_bar_for_mode_to_buffer(&config, mode, width, 16)
            });
            assert!(
                result.is_ok(),
                "bottom-bar rendering must not panic in {mode:?} at width {width}"
            );
            rendered.push(result.expect("checked above"));
        }

        let command_present = find_ascii_text(&rendered[0], COMMAND_CHIP).is_some();
        let typing_present = find_ascii_text(&rendered[1], TYPING_CHIP).is_some();
        assert_eq!(
            command_present, typing_present,
            "COMMAND and TYPING chip visibility must be symmetric at width {width}"
        );
        assert_eq!(
            command_present,
            width >= 10,
            "both mode chips must be absent below the shared 10-column threshold and present at or above it (width {width})"
        );
    }

    for (width, reserved_rows) in [(19, 11), (40, 5), (80, 3), (120, 2), (200, 1)] {
        let tall_bar = render_button_bar_for_mode_to_buffer(&config, UiMode::Normal, width, 16);
        assert_eq!(
            nonblank_rows(&tall_bar),
            usize::from(reserved_rows),
            "the unconstrained {width}-column command bar must naturally render {reserved_rows} rows"
        );

        let reserved_bar =
            render_button_bar_for_mode_to_buffer(&config, UiMode::Normal, width, reserved_rows);
        assert_eq!(
            nonblank_rows(&reserved_bar),
            usize::from(reserved_rows),
            "the {width}-column command bar must render into every one of its {reserved_rows} reserved rows"
        );
    }
}

/// Scenario: Render a roomy focused pane with readable agent output in freshly-entered command mode, then render the same pane in PaneInput and as an unfocused command-mode pane. Only the focused command pane must keep its content while dimming the inner area and centring the large COMMAND MODE banner with its Ctrl+D subtitle; snapshots pin the command and typing states.
#[spec("mode/banner/001")]
#[test]
fn mode_banner_001_big_banner_dims_only_the_focused_command_pane() {
    const CONTENT: &[u8] = b"FOCUSED AGENT OUTPUT remains readable\r\nanother status line";
    const WIDTH: u16 = 66;
    const HEIGHT: u16 = 15;

    let command = render_command_banner_pane_to_buffer(
        UiMode::Normal,
        true,
        CommandBannerVisibility::Expanded,
        CONTENT,
        WIDTH,
        HEIGHT,
    );
    let typing = render_command_banner_pane_to_buffer(
        UiMode::PaneInput,
        true,
        CommandBannerVisibility::Expanded,
        CONTENT,
        WIDTH,
        HEIGHT,
    );
    let unfocused = render_command_banner_pane_to_buffer(
        UiMode::Normal,
        false,
        CommandBannerVisibility::Expanded,
        CONTENT,
        WIDTH,
        HEIGHT,
    );

    assert_eq!(
        command_banner_tier(WIDTH - 2, HEIGHT - 2),
        BannerTier::BlockCommandModeWithSubtitle,
        "roomy fixture must exercise the full block-letter tier"
    );
    let banner = assert_banner_within_inner_area(&command, "roomy command pane")
        .expect("fresh command mode must render a REVERSED block banner");
    let inner = inner_rect(&command);
    let left_margin = banner.x - inner.x;
    let right_margin = inner.x + inner.width - (banner.x + banner.width);
    let top_margin = banner.y - inner.y;
    let bottom_margin = inner.y + inner.height - (banner.y + banner.height);
    assert!(
        left_margin.abs_diff(right_margin) <= 1 && top_margin.abs_diff(bottom_margin) <= 1,
        "block banner must be centred in {inner:?}, got {banner:?} with margins left={left_margin}, right={right_margin}, top={top_margin}, bottom={bottom_margin}"
    );
    assert!(
        banner.width >= 40 && banner.height >= 5,
        "tier 1 must visibly occupy a block-letter region, got {banner:?}"
    );
    let (subtitle_x, subtitle_y) = find_ascii_text(&command, "Ctrl+D to type")
        .expect("tier 1 must render the Ctrl+D to type subtitle");
    assert_eq!(
        subtitle_x,
        inner.x + (inner.width - "Ctrl+D to type".len() as u16) / 2,
        "subtitle must be horizontally centred"
    );
    assert!(subtitle_y >= banner.y && subtitle_y < banner.y + banner.height);

    assert_command_inner_area_is_dimmed(&command, "focused command pane");
    let (content_x, content_y) = find_ascii_text(&command, "FOCUSED AGENT OUTPUT")
        .expect("dimming must leave the underlying agent output legible, not erase it");
    for x in content_x..content_x + "FOCUSED AGENT OUTPUT".len() as u16 {
        assert!(
            command[(x, content_y)].modifier.contains(Modifier::DIM),
            "underlying readable content cell ({x}, {content_y}) must carry DIM"
        );
    }
    assert_no_rgb(&command, "command banner");

    assert!(find_ascii_text(&typing, "FOCUSED AGENT OUTPUT").is_some());
    assert!(
        modifier_bounds(&typing, Modifier::REVERSED).is_none(),
        "PaneInput must never render the command banner"
    );
    assert_inner_area_has_no_modifier(&typing, Modifier::DIM, "PaneInput pane");

    assert!(find_ascii_text(&unfocused, "FOCUSED AGENT OUTPUT").is_some());
    assert!(
        modifier_bounds(&unfocused, Modifier::REVERSED).is_none(),
        "an unfocused command-mode pane must not render the banner"
    );
    assert_inner_area_has_no_modifier(&unfocused, Modifier::DIM, "unfocused command pane");

    insta::assert_snapshot!(
        "mode_banner_001_big_and_typing_states",
        format!(
            "COMMAND MODE\nPLAIN\n{}\nSTYLED\n{}\nPANE INPUT\nPLAIN\n{}\nSTYLED\n{}",
            buffer_to_text(&command),
            buffer_to_styled_text(&command),
            buffer_to_text(&typing),
            buffer_to_styled_text(&typing),
        )
    );
}

fn banner_tier_rank(tier: BannerTier) -> u8 {
    match tier {
        BannerTier::BlockCommandModeWithSubtitle => 1,
        BannerTier::BlockCommand => 2,
        BannerTier::SingleLineWithSubtitle => 3,
        BannerTier::SingleWord => 4,
        BannerTier::Omitted => 5,
    }
}

/// Scenario: Sweep command-banner inner-area dimensions from degenerate through roomy and ask the production pure selector which fallback tier fits. Every tier must own a reachable size band, every selected rendering must fit, and adding width or height must never degrade the choice.
#[spec("mode/banner/002")]
#[test]
fn mode_banner_002_narrow_fallback_ladder_is_monotonic_and_fits() {
    for (width, height, expected) in [
        (60, 7, BannerTier::BlockCommandModeWithSubtitle),
        (40, 5, BannerTier::BlockCommand),
        (31, 2, BannerTier::SingleLineWithSubtitle),
        (9, 2, BannerTier::SingleWord),
        (8, 2, BannerTier::Omitted),
    ] {
        assert_eq!(
            command_banner_tier(width, height),
            expected,
            "{width}x{height} must select its documented fallback band"
        );
    }

    for (width, height) in [(0, 0), (1, 1), (200, 1), (3, 200)] {
        assert_eq!(
            command_banner_tier(width, height),
            BannerTier::Omitted,
            "degenerate {width}x{height} inner area must safely omit the banner"
        );
    }

    for width in 0..=100 {
        for height in 0..=12 {
            let tier = command_banner_tier(width, height);
            let (rendered_width, rendered_height) = tier.rendered_size();
            assert!(
                rendered_width <= width && rendered_height <= height,
                "{tier:?} reports {rendered_width}x{rendered_height}, which does not fit the selected {width}x{height} inner area"
            );

            if width < 100 {
                let wider = command_banner_tier(width + 1, height);
                assert!(
                    banner_tier_rank(wider) <= banner_tier_rank(tier),
                    "widening {width}x{height} to {}x{height} degraded {tier:?} to {wider:?}",
                    width + 1
                );
            }
            if height < 12 {
                let taller = command_banner_tier(width, height + 1);
                assert!(
                    banner_tier_rank(taller) <= banner_tier_rank(tier),
                    "heightening {width}x{height} to {width}x{} degraded {tier:?} to {taller:?}",
                    height + 1
                );
            }
        }
    }
}

/// Scenario: Drive the command-banner state with an injected Instant through fresh entry, deterministic TTL expiry, bound actions, unbound printable keys, a bottom-bar click, leave, and re-entry. Bound actions, clicks, and expiry collapse the banner; an unbound printable holds or re-asserts it, and every fresh command-mode entry re-arms it.
#[spec("mode/banner/003")]
#[test]
fn mode_banner_003_decay_state_is_deterministic_and_rearms() {
    assert_eq!(COMMAND_BANNER_TTL, Duration::from_millis(2500));

    let epoch = Instant::now();
    let mut state = CommandBannerState::default();

    state.handle(CommandBannerSignal::EnterCommandMode, epoch);
    assert_eq!(
        state.visibility(epoch),
        CommandBannerVisibility::Expanded,
        "fresh command-mode entry must show the big banner"
    );
    assert_eq!(
        state.visibility(epoch + COMMAND_BANNER_TTL - Duration::from_millis(1)),
        CommandBannerVisibility::Expanded,
        "the banner must remain expanded while its entry Instant is younger than the TTL"
    );
    assert_eq!(
        state.visibility(epoch + COMMAND_BANNER_TTL),
        CommandBannerVisibility::Collapsed,
        "reaching the TTL must deterministically collapse and latch the banner without sleeping"
    );

    let bound_entry = epoch + Duration::from_secs(10);
    state.handle(CommandBannerSignal::EnterCommandMode, bound_entry);
    state.handle(
        CommandBannerSignal::CommandAction,
        bound_entry + Duration::from_millis(100),
    );
    assert_eq!(
        state.visibility(bound_entry + Duration::from_millis(100)),
        CommandBannerVisibility::Collapsed,
        "a key that resolved to a command-mode Action must collapse before the TTL"
    );

    let click_entry = epoch + Duration::from_secs(20);
    state.handle(CommandBannerSignal::EnterCommandMode, click_entry);
    state.handle(
        CommandBannerSignal::BottomBarClick,
        click_entry + Duration::from_millis(100),
    );
    assert_eq!(
        state.visibility(click_entry + Duration::from_millis(100)),
        CommandBannerVisibility::Collapsed,
        "a bottom-bar button click must collapse the banner"
    );

    let unbound_entry = epoch + Duration::from_secs(30);
    state.handle(CommandBannerSignal::EnterCommandMode, unbound_entry);
    state.handle(
        CommandBannerSignal::UnboundPrintable,
        unbound_entry + Duration::from_millis(100),
    );
    assert_eq!(
        state.visibility(unbound_entry + Duration::from_millis(100)),
        CommandBannerVisibility::Expanded,
        "an unbound printable before the TTL must hold the big signal"
    );

    state.handle(
        CommandBannerSignal::CommandAction,
        unbound_entry + Duration::from_millis(200),
    );
    assert_eq!(
        state.visibility(unbound_entry + Duration::from_millis(200)),
        CommandBannerVisibility::Collapsed,
    );
    state.handle(
        CommandBannerSignal::UnboundPrintable,
        unbound_entry + Duration::from_millis(300),
    );
    assert_eq!(
        state.visibility(unbound_entry + Duration::from_millis(300)),
        CommandBannerVisibility::Expanded,
        "an unbound printable after collapse must re-assert the big banner with a fresh clock"
    );

    state.handle(
        CommandBannerSignal::LeaveCommandMode,
        unbound_entry + Duration::from_millis(400),
    );
    assert_eq!(
        state.visibility(unbound_entry + Duration::from_millis(400)),
        CommandBannerVisibility::Hidden,
        "leaving command mode must clear the banner state"
    );
    let reentry = epoch + Duration::from_secs(40);
    state.handle(CommandBannerSignal::EnterCommandMode, reentry);
    assert_eq!(
        state.visibility(reentry),
        CommandBannerVisibility::Expanded,
        "each fresh command-mode entry must re-arm the big banner"
    );
}

/// Scenario: Render nonempty command panes at degenerate, oversized, degraded-banner, and collapsed-banner sizes. Every render must survive with the exact requested buffer dimensions; valid bordered panes must keep the banner inside the pane, retain DIM, and leave the persistent COMMAND chip as the collapsed signal, with a snapshot pinning all degraded states.
#[spec("mode/banner/004")]
#[test]
fn mode_banner_004_degraded_and_collapsed_states_never_escape_the_pane() {
    const CONTENT: &[u8] = b"small pane output";

    let oversized_render = EmbeddedPaneController::for_render_seam_with_focused_pane(
        "oversized-render",
        PTY_RESIZE_DIM_MAX + 1,
        40,
        CONTENT,
    );
    let oversized_render_size = oversized_render
        .get_screen("oversized-render")
        .expect("the render seam must register its focused pane")
        .lock()
        .expect("the render seam screen lock must remain healthy")
        .screen()
        .size();
    assert_eq!(
        oversized_render_size,
        (24, 80),
        "an oversized render-seam axis must use parser_init_dims' safe fallback"
    );

    let (oversized_scroll, _child_input) =
        EmbeddedPaneController::for_scroll_seam_with_focused_pane(
            "oversized-scroll",
            40,
            PTY_RESIZE_DIM_MAX + 1,
            CONTENT,
            false,
        );
    let oversized_scroll_size = oversized_scroll
        .get_screen("oversized-scroll")
        .expect("the scroll seam must register its focused pane")
        .lock()
        .expect("the scroll seam screen lock must remain healthy")
        .screen()
        .size();
    assert_eq!(
        oversized_scroll_size,
        (24, 80),
        "an oversized scroll-seam axis must use parser_init_dims' safe fallback"
    );

    let _second_child_input = oversized_scroll.add_scroll_seam_pane(
        "oversized-second",
        PTY_RESIZE_DIM_MAX + 1,
        40,
        CONTENT,
    );
    let oversized_second_size = oversized_scroll
        .get_screen("oversized-second")
        .expect("the second-pane seam must register its pane")
        .lock()
        .expect("the second-pane seam screen lock must remain healthy")
        .screen()
        .size();
    assert_eq!(
        oversized_second_size,
        (24, 80),
        "an oversized second-pane axis must use parser_init_dims' safe fallback"
    );

    for (width, height) in [(1, 1), (0, 0), (2, 2), (1, 40), (40, 1)] {
        let rendered = std::panic::catch_unwind(|| {
            render_command_banner_pane_to_buffer(
                UiMode::Normal,
                true,
                CommandBannerVisibility::Expanded,
                CONTENT,
                width,
                height,
            )
        });
        assert!(
            rendered.is_ok(),
            "nonempty pane rendering must not panic at degenerate or oversized {width}x{height} dimensions"
        );
        let buffer = rendered.expect("checked above");
        assert_eq!(
            (buffer.area().width, buffer.area().height),
            (width, height),
            "the renderer must return the exact requested {width}x{height} buffer"
        );
    }

    let cases = [
        ("BLOCK COMMAND", 40, 5, BannerTier::BlockCommand, true),
        (
            "SINGLE LINE",
            31,
            2,
            BannerTier::SingleLineWithSubtitle,
            true,
        ),
        ("SINGLE WORD", 9, 2, BannerTier::SingleWord, true),
        ("OMITTED", 8, 2, BannerTier::Omitted, false),
    ];
    let mut snapshot = String::new();

    for (label, inner_width, inner_height, expected_tier, expects_banner) in cases {
        assert_eq!(
            command_banner_tier(inner_width, inner_height),
            expected_tier,
            "{label} fixture must select the intended tier"
        );
        let width = inner_width + 2;
        let height = inner_height + 2;
        let rendered = std::panic::catch_unwind(|| {
            render_command_banner_pane_to_buffer(
                UiMode::Normal,
                true,
                CommandBannerVisibility::Expanded,
                CONTENT,
                width,
                height,
            )
        });
        assert!(
            rendered.is_ok(),
            "{label} banner must not panic in its small-but-valid {width}x{height} pane"
        );
        let buffer = rendered.expect("checked above");
        assert_eq!((buffer.area().width, buffer.area().height), (width, height));
        let bounds = assert_banner_within_inner_area(&buffer, label);
        assert_eq!(
            bounds.is_some(),
            expects_banner,
            "{label} must render a banner exactly when its tier is not Omitted"
        );
        assert_command_inner_area_is_dimmed(&buffer, label);
        assert_no_rgb(&buffer, label);

        match expected_tier {
            BannerTier::BlockCommand => {
                assert!(find_ascii_text(&buffer, "Ctrl+D to type").is_none());
                assert!(bounds.is_some_and(|rect| rect.height >= 5));
            }
            BannerTier::SingleLineWithSubtitle => {
                assert!(
                    find_ascii_text(&buffer, " COMMAND MODE — Ctrl+D to type ").is_some(),
                    "tier 3 must use the complete single reversed line"
                );
            }
            BannerTier::SingleWord => {
                assert!(
                    find_ascii_text(&buffer, " COMMAND ").is_some(),
                    "tier 4 must use the single reversed word"
                );
                assert!(find_ascii_text(&buffer, "MODE").is_none());
            }
            BannerTier::Omitted => {
                assert!(
                    find_ascii_text(&buffer, "COMMAND").is_none(),
                    "tier 5 must omit banner text while retaining DIM"
                );
            }
            BannerTier::BlockCommandModeWithSubtitle => {
                panic!("tier 1 is covered by mode/banner/001, not a degraded fixture")
            }
        }

        let _ = writeln!(
            snapshot,
            "{label} ({width}x{height})\nPLAIN\n{}\nSTYLED\n{}",
            buffer_to_text(&buffer),
            buffer_to_styled_text(&buffer)
        );
    }

    let collapsed = render_command_banner_pane_to_buffer(
        UiMode::Normal,
        true,
        CommandBannerVisibility::Collapsed,
        CONTENT,
        66,
        15,
    );
    assert!(
        modifier_bounds(&collapsed, Modifier::REVERSED).is_none(),
        "collapsed-after-decay pane must remove only the banner"
    );
    assert_command_inner_area_is_dimmed(&collapsed, "collapsed command pane");
    assert!(
        find_ascii_text(&collapsed, "small pane output").is_some(),
        "collapsed command pane must keep readable content"
    );

    let chip =
        render_button_bar_for_mode_to_buffer(&KeybindingConfig::default(), UiMode::Normal, 120, 2);
    assert_mode_chip_at_origin(&chip, COMMAND_CHIP, "collapsed command-mode frame");
    let _ = writeln!(
        snapshot,
        "COLLAPSED AFTER DECAY\nPLAIN\n{}\nSTYLED\n{}\nPERSISTENT CHIP\n{}",
        buffer_to_text(&collapsed),
        buffer_to_styled_text(&collapsed),
        buffer_to_text(&chip)
    );

    insta::assert_snapshot!("mode_banner_004_degraded_and_collapsed", snapshot);
}

/// Scenario: Feed five queued key bursts through the production per-key handler with a frame only before and after each burst. The two defect cases must observe their real intermediate modes and produce the opposite final banner states, while three controls pin ordinary bound, unbound-printable, and single-entry behavior.
#[spec("mode/banner/005")]
#[test]
fn mode_banner_005_same_drain_mode_edges_preserve_banner_semantics() {
    let config = KeybindingConfig::default();
    let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
    let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
    let printable = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);

    let cases = [
        (
            "double Ctrl+D round trip",
            UiMode::Normal,
            vec![ctrl_d, ctrl_d],
            vec![UiMode::PaneInput, UiMode::Normal],
            CommandBannerVisibility::Expanded,
            CommandBannerVisibility::Expanded,
        ),
        (
            "bound key immediately after PaneInput exits",
            UiMode::PaneInput,
            vec![ctrl_d, ctrl_t],
            vec![UiMode::Normal, UiMode::Normal],
            CommandBannerVisibility::Hidden,
            CommandBannerVisibility::Collapsed,
        ),
        (
            "single bound command key",
            UiMode::Normal,
            vec![ctrl_t],
            vec![UiMode::Normal],
            CommandBannerVisibility::Expanded,
            CommandBannerVisibility::Collapsed,
        ),
        (
            "unbound printable after a bound command key",
            UiMode::Normal,
            vec![ctrl_t, printable],
            vec![UiMode::Normal, UiMode::Normal],
            CommandBannerVisibility::Expanded,
            CommandBannerVisibility::Expanded,
        ),
        (
            "single entry from PaneInput",
            UiMode::PaneInput,
            vec![ctrl_d],
            vec![UiMode::Normal],
            CommandBannerVisibility::Hidden,
            CommandBannerVisibility::Expanded,
        ),
    ];

    for (context, initial_mode, keys, expected_modes, before, after) in cases {
        let observed = observe_command_banner_key_burst(&config, initial_mode, &keys);
        assert_eq!(
            observed.modes, expected_modes,
            "{context} must traverse the intended real modes"
        );
        assert_eq!(
            observed.visibility_before, before,
            "{context} must start from the banner state rendered before the drain"
        );
        assert_eq!(
            observed.visibility_after, after,
            "{context} must leave the first post-drain frame in the expected banner state"
        );
    }
}

/// Scenario: Render the same selected Dashboard card in command mode and PaneInput through the production card renderer. Both must keep the `▸ ` marker, a border in the terminal's own foreground, and a thick glyph; only the emphasis differs, command mode adding BOLD where PaneInput adds nothing. Neither may carry `Modifier::DIM`, since fading the selection is what made it read as an idle card; a colour-and-modifier-aware snapshot pins both states.
#[spec("mode/deck/001")]
#[test]
fn mode_deck_001_selected_card_accent_tracks_mode() {
    let session = selected_card_fixture();
    let width = 80;
    let density = CardDensityKind::Normal;
    let height = density.rendered_height();
    let command = render_card_for_mode_to_buffer(
        &session,
        Some("mode-card"),
        Some(1),
        density,
        0,
        true,
        UiMode::Normal,
        width,
        height,
    );
    let typing = render_card_for_mode_to_buffer(
        &session,
        Some("mode-card"),
        Some(1),
        density,
        0,
        true,
        UiMode::PaneInput,
        width,
        height,
    );
    let legacy = render_card_to_buffer(
        &session,
        Some("mode-card"),
        Some(1),
        density,
        0,
        true,
        width,
        height,
    );

    assert_eq!(
        command, legacy,
        "command-mode selection must stay byte-identical to today's selected-card rendering"
    );

    let border_y = height / 2;
    let command_border = &command[(0, border_y)];
    let typing_border = &typing[(0, border_y)];

    // Issue #442: a SELECTED card's border is the terminal's own foreground in
    // BOTH modes — the one colour that contrasts on a light and a dark theme
    // alike. Resting on the status colour would make a selected idle card
    // (DarkGray) invisible on a dark background however thick its border was.
    for (label, cell) in [("command", command_border), ("PaneInput", typing_border)] {
        assert_eq!(
            cell.fg,
            Color::Reset,
            "{label}-mode selected card must use the terminal's own foreground, got {:?}",
            cell.fg
        );
        assert_eq!(
            cell.symbol(),
            "┃",
            "{label}-mode selected card must draw a THICK border, got {:?}",
            cell.symbol()
        );
        assert!(
            !cell.modifier.contains(Modifier::DIM),
            "{label}-mode selection must never be DIM — that is what made the \
             selected card read as an idle one (issue #442), got {:?}",
            cell.style()
        );
    }

    assert!(
        buffer_to_text(&command).contains("▸ "),
        "command mode must retain the selected-card title marker"
    );
    assert!(
        buffer_to_text(&typing).contains("▸ "),
        "PaneInput must retain the selected-card title marker"
    );

    // The mode still has to be legible, and it rides on emphasis alone now.
    assert_ne!(
        typing_border.style(),
        command_border.style(),
        "PaneInput selection must be visibly de-emphasised relative to command mode"
    );
    assert!(
        command_border.modifier.contains(Modifier::BOLD),
        "command mode drives the deck, so its selection carries BOLD, got {:?}",
        command_border.style()
    );
    assert!(
        !typing_border.modifier.contains(Modifier::BOLD),
        "PaneInput drives a pane, so its selection drops BOLD, got {:?}",
        typing_border.style()
    );
    assert_no_rgb(&command, "command-mode selected card");
    assert_no_rgb(&typing, "PaneInput selected card");

    insta::assert_snapshot!(
        "mode_deck_001_selected_card_styles",
        format!(
            "COMMAND MODE\n{}\nPANE INPUT\n{}",
            buffer_to_styled_text(&command),
            buffer_to_styled_text(&typing)
        )
    );
}

/// Scenario: Send one wheel-up event over the focused agent pane in all four combinations of command/PaneInput mode and child mouse reporting on/off. PaneInput may forward only when reporting is enabled; command mode must always move dot-agent-deck's scrollback and must never emit mouse-protocol bytes to the child.
#[spec("mode/scroll/001")]
#[test]
fn mode_scroll_001_mouse_wheel_routes_by_mode_and_child_mouse_state() {
    for (mode, mouse_mode_enabled, should_forward, context) in [
        (
            UiMode::PaneInput,
            true,
            true,
            "PaneInput with child mouse reporting",
        ),
        (
            UiMode::PaneInput,
            false,
            false,
            "PaneInput without child mouse reporting",
        ),
        (
            UiMode::Normal,
            true,
            false,
            "command mode with child mouse reporting",
        ),
        (
            UiMode::Normal,
            false,
            false,
            "command mode without child mouse reporting",
        ),
    ] {
        let observed = observe_focused_agent_mouse_scroll(mode, mouse_mode_enabled, true, 0, 3, 2);
        if should_forward {
            assert_eq!(
                observed.forwarded_bytes, b"\x1b[<64;4;3M",
                "{context} must forward exactly one wheel-up report to the child"
            );
            assert_eq!(
                observed.scrollback_after, observed.scrollback_before,
                "{context} must leave dot-agent-deck's scrollback at live output"
            );
        } else {
            assert!(
                observed.scrollback_after > observed.scrollback_before,
                "{context} must scroll dot-agent-deck's own scrollback: {observed:?}"
            );
            assert!(
                observed.forwarded_bytes.is_empty(),
                "{context} must not emit mouse-protocol bytes to the child: {observed:?}"
            );
        }
    }
}

/// Scenario: In command mode, press the configured focused-pane scroll-up and scroll-down keys against a pane with synthetic history. The PageUp/PageDown defaults and custom `[dashboard]` remaps must move dot-agent-deck's scrollback in the expected direction without emitting bytes to the child, while each remap disables its former default.
#[spec("mode/scroll/002")]
#[test]
fn mode_scroll_002_keyboard_scroll_is_semantic_and_remappable() {
    let default = KeybindingConfig::default();
    let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
    let page_down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);

    let up = observe_focused_agent_key_scroll(&default, UiMode::Normal, page_up, 0);
    assert!(
        up.scrollback_after > up.scrollback_before,
        "default PageUp must scroll back through the focused agent pane: {up:?}"
    );
    assert!(
        up.forwarded_bytes.is_empty(),
        "command-mode PageUp must not emit bytes to the child: {up:?}"
    );

    let down = observe_focused_agent_key_scroll(&default, UiMode::Normal, page_down, 6);
    assert!(
        down.scrollback_after < down.scrollback_before,
        "default PageDown must scroll toward live output in the focused agent pane: {down:?}"
    );
    assert!(
        down.forwarded_bytes.is_empty(),
        "command-mode PageDown must not emit bytes to the child: {down:?}"
    );

    let (remapped, warnings) = KeybindingConfig::from_toml_str(
        "[dashboard]\nscroll_pane_up = \"Alt+u\"\nscroll_pane_down = \"Alt+d\"\n",
    )
    .expect("focused-pane scroll remaps must parse");
    assert!(
        warnings.is_empty(),
        "focused-pane scroll keys must be registered semantic actions: {warnings:?}"
    );

    let old_up = observe_focused_agent_key_scroll(&remapped, UiMode::Normal, page_up, 0);
    assert_eq!(
        old_up.scrollback_after, old_up.scrollback_before,
        "remapping scroll_pane_up must disable its PageUp default"
    );
    let remapped_up = observe_focused_agent_key_scroll(
        &remapped,
        UiMode::Normal,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::ALT),
        0,
    );
    assert!(
        remapped_up.scrollback_after > remapped_up.scrollback_before,
        "the custom scroll_pane_up binding must scroll the focused pane: {remapped_up:?}"
    );

    let old_down = observe_focused_agent_key_scroll(&remapped, UiMode::Normal, page_down, 6);
    assert_eq!(
        old_down.scrollback_after, old_down.scrollback_before,
        "remapping scroll_pane_down must disable its PageDown default"
    );
    let remapped_down = observe_focused_agent_key_scroll(
        &remapped,
        UiMode::Normal,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
        6,
    );
    assert!(
        remapped_down.scrollback_after < remapped_down.scrollback_before,
        "the custom scroll_pane_down binding must scroll the focused pane: {remapped_down:?}"
    );
    assert!(
        remapped_up.forwarded_bytes.is_empty() && remapped_down.forwarded_bytes.is_empty(),
        "command-mode keyboard scrolling must never write to the child"
    );
}

/// Scenario: Scroll one of two synthetic panes away from live output between consecutive frames, then enter PaneInput or remain in the current mode while optionally changing focus. A newly targeted PaneInput pane must snap back to live output, while an unchanged PaneInput target and command-mode pane must deliberately retain their offsets.
#[spec("mode/scroll/003")]
#[test]
fn mode_scroll_003_pane_input_target_change_snaps_scrollback_to_live_output() {
    let resumed_typing = observe_pane_input_scrollback_reconcile(
        UiMode::Normal,
        ReconcileSeamPane::First,
        Some((ReconcileSeamPane::First, 3)),
        UiMode::PaneInput,
        ReconcileSeamPane::First,
    );
    assert_eq!(
        (
            resumed_typing.first_before,
            resumed_typing.first_after,
            resumed_typing.second_before,
            resumed_typing.second_after,
        ),
        (3, 0, 0, 0),
        "entering PaneInput must snap the focused pane from verified scrollback to live output"
    );

    let unchanged_typing = observe_pane_input_scrollback_reconcile(
        UiMode::PaneInput,
        ReconcileSeamPane::First,
        Some((ReconcileSeamPane::First, 3)),
        UiMode::PaneInput,
        ReconcileSeamPane::First,
    );
    assert_eq!(
        (
            unchanged_typing.first_before,
            unchanged_typing.first_after,
            unchanged_typing.second_before,
            unchanged_typing.second_after,
        ),
        (3, 3, 0, 0),
        "an unchanged PaneInput target must retain its offset so PaneInput scrolling remains possible"
    );

    let newly_focused_typing = observe_pane_input_scrollback_reconcile(
        UiMode::PaneInput,
        ReconcileSeamPane::First,
        Some((ReconcileSeamPane::Second, 3)),
        UiMode::PaneInput,
        ReconcileSeamPane::Second,
    );
    assert_eq!(
        (
            newly_focused_typing.first_before,
            newly_focused_typing.first_after,
            newly_focused_typing.second_before,
            newly_focused_typing.second_after,
        ),
        (0, 0, 3, 0),
        "moving PaneInput focus must snap only the newly targeted pane to live output"
    );

    let unchanged_command = observe_pane_input_scrollback_reconcile(
        UiMode::Normal,
        ReconcileSeamPane::First,
        Some((ReconcileSeamPane::First, 3)),
        UiMode::Normal,
        ReconcileSeamPane::First,
    );
    assert_eq!(
        (
            unchanged_command.first_before,
            unchanged_command.first_after,
            unchanged_command.second_before,
            unchanged_command.second_after,
        ),
        (3, 3, 0, 0),
        "an unchanged command-mode pane must retain its offset so command-mode scrolling remains possible"
    );
}

/// Scenario: Render two consecutive no-focus frames from PaneInput and from an already-Normal mode using injected frame instants. The first frame must drop safely to command mode and expand its banner exactly once; one TTL later the second frame must remain Normal with a Collapsed banner, while equal instants keep it Expanded without re-entering.
#[spec("mode/scroll/004")]
#[test]
fn mode_scroll_004_pane_input_without_focus_settles_in_command_mode_once() {
    let epoch = Instant::now();
    let settled = observe_pane_input_without_focused_pane(
        UiMode::PaneInput,
        epoch,
        epoch + COMMAND_BANNER_TTL,
    );
    assert_eq!(
        (
            settled.mode_after_first_frame,
            settled.banner_after_first_frame,
            settled.mode_after_second_frame,
            settled.banner_after_second_frame,
        ),
        (
            UiMode::Normal,
            CommandBannerVisibility::Expanded,
            UiMode::Normal,
            CommandBannerVisibility::Collapsed,
        ),
        "PaneInput without focus must enter command mode once, then let the banner collapse at the TTL instead of re-stamping it every frame"
    );

    let same_instant = observe_pane_input_without_focused_pane(UiMode::PaneInput, epoch, epoch);
    assert_eq!(
        (
            same_instant.mode_after_first_frame,
            same_instant.banner_after_first_frame,
            same_instant.mode_after_second_frame,
            same_instant.banner_after_second_frame,
        ),
        (
            UiMode::Normal,
            CommandBannerVisibility::Expanded,
            UiMode::Normal,
            CommandBannerVisibility::Expanded,
        ),
        "without elapsed time the settled command banner must remain expanded across the second frame"
    );

    let already_normal =
        observe_pane_input_without_focused_pane(UiMode::Normal, epoch, epoch + COMMAND_BANNER_TTL);
    assert_eq!(
        (
            already_normal.mode_after_first_frame,
            already_normal.banner_after_first_frame,
            already_normal.mode_after_second_frame,
            already_normal.banner_after_second_frame,
        ),
        (
            settled.mode_after_first_frame,
            settled.banner_after_first_frame,
            settled.mode_after_second_frame,
            settled.banner_after_second_frame,
        ),
        "reconciling an already-Normal no-focus state must be idempotent"
    );
}
