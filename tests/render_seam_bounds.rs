//! L1 guards on the allocation bound the exported `*_to_buffer` render seams
//! with TWO caller-controlled axes apply to their dimensions (issue #748).
//!
//! `src/lib.rs` is `pub mod ui;`, so each of these seams is reachable as
//! `dot_agent_deck::ui::render_*_to_buffer` by anything that depends on the
//! crate — and half of them carry no `#[doc(hidden)]` at all. Each builds a
//! `ratatui::backend::TestBackend`, which allocates exactly one cell per
//! `width * height`, so before the bound a call at `u16::MAX` on both axes
//! asked for ~4.3 BILLION cells: a process abort rather than an error a caller
//! could handle. These tests assert the bound, in-process, through the real
//! exported entry points.
//!
//! **The two-axis qualifier is the scope, and the last test pins the other
//! side of it.** Four exported seams pass a literal `1` as their height, so
//! their worst case is 65,535 cells and issue #748 deliberately left them
//! unbounded; `seam_bound_001_one_row_seams_are_deliberately_unbounded` asserts
//! that, so "not bounded" stays a recorded decision instead of decaying into an
//! oversight nobody can tell from a bug.

use std::collections::VecDeque;

use dot_agent_deck::event::AgentType;
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::state::{DashboardStats, SessionState, SessionStatus};
use dot_agent_deck::ui::{
    CardDensityKind, CommandBannerVisibility, RENDER_SEAM_DIM_MAX, UiMode,
    render_button_bar_for_mode_to_buffer, render_button_bar_to_buffer,
    render_button_bar_with_bindings_to_buffer, render_card_grid_to_buffer, render_card_to_buffer,
    render_card_with_declared_agent_to_buffer, render_command_banner_pane_to_buffer,
    render_dashboard_cards_to_buffer, render_filter_bar_to_buffer, render_help_overlay_to_buffer,
    render_help_overlay_with_bindings_to_buffer, render_hints_bar_for_mode_to_buffer,
    render_hints_bar_to_buffer, render_quit_confirm_to_buffer, render_rename_bar_to_buffer,
    render_stats_bar_to_buffer, render_tab_bar_to_buffer,
};
use spec::spec;

/// A minimal live session for the card seams. Only the geometry matters here —
/// no assertion in this file reads a rendered cell — so the fields carry
/// whatever keeps `render_session_card` on its ordinary path.
fn fixture_session() -> SessionState {
    let now = chrono::Utc::now();
    SessionState {
        session_id: "sess-bound".to_string(),
        agent_type: AgentType::ClaudeCode,
        cwd: Some("/home/dev/example-project".to_string()),
        status: SessionStatus::Working,
        active_tool: None,
        started_at: now,
        last_activity: now,
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: None,
        first_prompts: Vec::new(),
        pane_id: Some("pane-1".to_string()),
        agent_id: Some("1".to_string()),
        display_name: None,
        shell_synthetic_working: false,
    }
}

/// The size of a rendered buffer, as `(width, height)`.
fn dims(buffer: &ratatui::buffer::Buffer) -> (u16, u16) {
    (buffer.area().width, buffer.area().height)
}

/// One exported one-row bar seam: a width in, the rendered `(width, height)`
/// out. Named because `clippy::type_complexity` refuses the tuple inline.
type OneRowSeam = (&'static str, Box<dyn Fn(u16) -> (u16, u16)>);

/// One exported seam, reduced to "given these dimensions, what size buffer did
/// you actually hand back".
struct Seam {
    name: &'static str,
    render: Box<dyn Fn(u16, u16) -> (u16, u16)>,
}

/// Exported seams whose `TestBackend` is sized from two caller-given dimensions.
///
/// Twelve entry points covering nine of the ten places the bound is applied:
/// the two shared helpers (`draw_to_buffer`, reached here through the stats-bar
/// and quit-confirm wrappers, and `render_overlay_to_buffer`, reached through
/// the help overlay) and seven of the eight seams that construct their own
/// backend. The eighth, `render_dashboard_cards_to_buffer`, takes only a width —
/// its height comes from the card count — so it does not fit this shape and has
/// its own test below. `render_hints_bar_to_buffer` and `render_card_to_buffer`
/// are here even though they allocate nothing themselves: they delegate, one and
/// two hops respectively, and a seam that reached an unbounded callee would be
/// just as bad.
fn bounded_seams() -> Vec<Seam> {
    fn seam(name: &'static str, render: impl Fn(u16, u16) -> (u16, u16) + 'static) -> Seam {
        Seam {
            name,
            render: Box::new(render),
        }
    }

    vec![
        // Group A — through the shared `draw_to_buffer` helper.
        seam("render_stats_bar_to_buffer", |w, h| {
            let stats = DashboardStats::default();
            let buffer = render_stats_bar_to_buffer(&stats, None, w, h);
            dims(&buffer)
        }),
        seam("render_quit_confirm_to_buffer", |w, h| {
            let buffer = render_quit_confirm_to_buffer(0, w, h);
            dims(&buffer)
        }),
        // Group B — through the shared `render_overlay_to_buffer` helper.
        seam("render_help_overlay_to_buffer", |w, h| {
            let buffer = render_help_overlay_to_buffer(w, h);
            dims(&buffer)
        }),
        // Group C — seams that build their own `TestBackend`.
        seam("render_help_overlay_with_bindings_to_buffer", |w, h| {
            let keybindings = KeybindingConfig::default();
            let buffer = render_help_overlay_with_bindings_to_buffer(&keybindings, None, w, h);
            dims(&buffer)
        }),
        seam("render_hints_bar_to_buffer", |w, h| {
            let keybindings = KeybindingConfig::default();
            let buffer = render_hints_bar_to_buffer(&keybindings, w, h);
            dims(&buffer)
        }),
        seam("render_hints_bar_for_mode_to_buffer", |w, h| {
            let keybindings = KeybindingConfig::default();
            let buffer = render_hints_bar_for_mode_to_buffer(&keybindings, UiMode::PaneInput, w, h);
            dims(&buffer)
        }),
        seam("render_card_to_buffer", |w, h| {
            let session = fixture_session();
            let buffer = render_card_to_buffer(
                &session,
                None,
                Some(1),
                CardDensityKind::Normal,
                0,
                false,
                w,
                h,
            );
            dims(&buffer)
        }),
        seam("render_card_with_declared_agent_to_buffer", |w, h| {
            let session = fixture_session();
            let buffer = render_card_with_declared_agent_to_buffer(
                &session,
                None,
                Some(1),
                CardDensityKind::Normal,
                0,
                false,
                UiMode::Normal,
                None,
                w,
                h,
            );
            dims(&buffer)
        }),
        seam("render_card_grid_to_buffer", |w, h| {
            let session = fixture_session();
            let cards = [(&session, None)];
            let (buffer, _probe) = render_card_grid_to_buffer(&cards, Some(0), 0, w, h);
            dims(&buffer)
        }),
        seam("render_button_bar_with_bindings_to_buffer", |w, h| {
            let keybindings = KeybindingConfig::default();
            let buffer = render_button_bar_with_bindings_to_buffer(&keybindings, w, h);
            dims(&buffer)
        }),
        seam("render_button_bar_for_mode_to_buffer", |w, h| {
            let keybindings = KeybindingConfig::default();
            let buffer = render_button_bar_for_mode_to_buffer(&keybindings, UiMode::Normal, w, h);
            dims(&buffer)
        }),
        seam("render_command_banner_pane_to_buffer", |w, h| {
            let buffer = render_command_banner_pane_to_buffer(
                UiMode::Normal,
                true,
                CommandBannerVisibility::Expanded,
                b"pane output",
                w,
                h,
            );
            dims(&buffer)
        }),
    ]
}

/// Scenario: Call twelve exported two-axis `*_to_buffer` render seams with
/// dimensions no terminal could have — `u16::MAX` on one axis, then on both —
/// and read the size of the buffer each one hands back. Each must come back
/// bounded to `RENDER_SEAM_DIM_MAX` on the oversized axes and untouched on the
/// in-range ones, rather than trying to allocate the request and aborting.
#[spec("render/seam-bound/001")]
#[test]
fn seam_bound_001_exported_seams_clamp_absurd_dimensions() {
    let cap = RENDER_SEAM_DIM_MAX;
    // The axis left in range while the other is absurd. Modest but not
    // degenerate — a seam's behaviour at a 1x1 or 4-column terminal is a
    // totality question, not an allocation one, and is not what this file pins.
    const SMALL_W: u16 = 24;
    const SMALL_H: u16 = 8;

    for Seam { name, render } in bounded_seams() {
        // An ordinary terminal is nowhere near the cap, so the bound must be
        // invisible to every real caller. This is also what fails if the clamp
        // is ever written as `max` rather than `min`.
        assert_eq!(
            render(80, 24),
            (80, 24),
            "{name}: an in-range 80x24 request must be honoured exactly"
        );

        // One axis at a time, with the other left small. Deliberate: an
        // unbounded seam allocates only ~65k cells for these, so a missing
        // clamp fails the assertion cleanly instead of asking the allocator
        // for the OOM this issue is about.
        assert_eq!(
            render(u16::MAX, SMALL_H),
            (cap, SMALL_H),
            "{name}: an absurd width must be bounded to {cap} and the height left alone"
        );
        assert_eq!(
            render(SMALL_W, u16::MAX),
            (SMALL_W, cap),
            "{name}: an absurd height must be bounded to {cap} and the width left alone"
        );
    }

    // Both axes oversized at once, which is the shape the issue was filed
    // about. Split out of the loop above and run over one seam per allocation
    // shape — the two shared helpers and one seam that builds its own backend —
    // because every call here renders the full 1024x1024 (~1M cells), and in a
    // debug build that is ~0.6s a time. Running it twelve times would make this
    // the slowest test in the fast tier to re-prove a property `min`-per-axis
    // already gives: the loop above has pinned both axes of all twelve.
    const BOTH_AXES: [&str; 3] = [
        "render_stats_bar_to_buffer",          // group A — `draw_to_buffer`
        "render_help_overlay_to_buffer",       // group B — `render_overlay_to_buffer`
        "render_hints_bar_for_mode_to_buffer", // group C — own `TestBackend`
    ];
    let heavy: Vec<Seam> = bounded_seams()
        .into_iter()
        .filter(|seam| BOTH_AXES.contains(&seam.name))
        .collect();
    assert_eq!(
        heavy.len(),
        BOTH_AXES.len(),
        "every name in BOTH_AXES must match a seam in bounded_seams()"
    );

    for Seam { name, render } in heavy {
        // One cell over the cap on both axes: unbounded this is ~1.05M cells, so
        // a missing clamp still fails the assertion rather than the allocator.
        assert_eq!(
            render(cap + 1, cap + 1),
            (cap, cap),
            "{name}: one cell over the cap on both axes must be bounded on both"
        );

        // Only now the call from the issue itself. Every assertion above has
        // already passed by the time this line runs, so the bound is known to be
        // in place and this allocates the capped 1024x1024 rather than the
        // 4,294,836,225 cells that made an unbounded seam a process abort. That
        // ordering is deliberate: a regression here is diagnosed by a failed
        // assertion earlier in this function, never by an OOM.
        assert_eq!(
            render(u16::MAX, u16::MAX),
            (cap, cap),
            "{name}: u16::MAX on both axes must return a bounded buffer, not abort"
        );
    }
}

/// Scenario: Render the stacked-card seam — the one whose height comes from the
/// card count rather than from an argument — at an absurd width and with more
/// cards than the cap allows rows for. Both axes of the returned buffer must be
/// bounded to `RENDER_SEAM_DIM_MAX`.
#[spec("render/seam-bound/001")]
#[test]
fn seam_bound_001_dashboard_cards_clamps_derived_height() {
    let cap = RENDER_SEAM_DIM_MAX;
    let density = CardDensityKind::Normal;
    let session = fixture_session();

    // In range on both axes: one card, honoured exactly.
    let one = [(&session, None)];
    let buffer = render_dashboard_cards_to_buffer(&one, Some(0), density, 0, 80);
    assert_eq!(
        dims(&buffer),
        (80, density.rendered_height()),
        "an in-range single-card render must be honoured exactly"
    );

    // An absurd width with a single card: unbounded this is ~0.5M cells, so a
    // missing clamp fails the assertion rather than the allocator.
    let buffer = render_dashboard_cards_to_buffer(&one, Some(0), density, 0, u16::MAX);
    assert_eq!(
        dims(&buffer),
        (cap, density.rendered_height()),
        "an absurd width must be bounded to {cap} and the derived height left alone"
    );

    // The height is `cards.len()` rows of `rendered_height()`, so enough cards
    // to overshoot the cap is how this seam's second axis gets absurd. The count
    // is derived from the density rather than hardcoded so it tracks the
    // production row budget. Paired with a modest width, so this too is only
    // ~25k cells unbounded.
    let over_cap = usize::from(cap / density.rendered_height()) + 2;
    let cards: Vec<(&SessionState, Option<&str>)> =
        (0..over_cap).map(|_| (&session, None)).collect();
    let buffer = render_dashboard_cards_to_buffer(&cards, Some(0), density, 0, 24);
    assert_eq!(
        dims(&buffer),
        (24, cap),
        "a card count past the cap must bound the derived height to {cap}"
    );

    // Both axes absurd at once. Reaching this line means the two assertions
    // above passed, so the bound is known to be in place and this allocates the
    // capped 1024x1024 — not the 68 MILLION cells `u16::MAX` by 130 cards' worth
    // of rows asks for unbounded. Same ordering as the sibling test: a
    // regression is diagnosed by an assertion above, never by the allocator.
    let buffer = render_dashboard_cards_to_buffer(&cards, Some(0), density, 0, u16::MAX);
    assert_eq!(
        dims(&buffer),
        (cap, cap),
        "an absurd width and a card count past the cap must both be bounded to {cap}"
    );
}

/// Scenario: Call the four exported one-row bar seams at `u16::MAX` columns and
/// confirm each returns a buffer that wide and exactly one row tall. They are
/// deliberately outside the `RENDER_SEAM_DIM_MAX` bound, and this pins that as a
/// decision: 65,535 cells is not an allocation concern, and a cap here would
/// silently truncate a legitimately wide bar.
#[spec("render/seam-bound/001")]
#[test]
fn seam_bound_001_one_row_seams_are_deliberately_unbounded() {
    // `u16::MAX * 1` = 65,535 cells, ~2 MB — the measurement issue #748 scoped
    // these out on, and the reason this test is cheap enough to make the
    // exclusion executable rather than leaving it as prose. Greptile flagged the
    // asymmetry on PR #841; the answer is that the bound is a two-axis rule, and
    // the way to keep that honest is to assert it in both directions.
    const WIDE: u16 = u16::MAX;

    let seams: [OneRowSeam; 4] = [
        (
            "render_button_bar_to_buffer",
            Box::new(|w| dims(&render_button_bar_to_buffer(w))),
        ),
        (
            "render_filter_bar_to_buffer",
            Box::new(|w| dims(&render_filter_bar_to_buffer("needle", w))),
        ),
        (
            "render_rename_bar_to_buffer",
            Box::new(|w| dims(&render_rename_bar_to_buffer("new-name", w))),
        ),
        (
            "render_tab_bar_to_buffer",
            Box::new(|w| {
                dims(&render_tab_bar_to_buffer(
                    &["dashboard"],
                    &[false],
                    0,
                    w,
                    &[None],
                ))
            }),
        ),
    ];

    for (name, render) in seams {
        assert_eq!(
            render(WIDE),
            (WIDE, 1),
            "{name}: a one-row seam honours its width verbatim and is NOT capped \
             at RENDER_SEAM_DIM_MAX ({RENDER_SEAM_DIM_MAX}) — if this now fails \
             because the seam was capped, that is a deliberate scope change and \
             RENDER_SEAM_DIM_MAX's doc comment has to move with it"
        );
    }
}
