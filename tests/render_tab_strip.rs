//! PRD #80 M3 — L1 widget test for the tab-strip close affordance.
//!
//! Per PRD #77 Decision 2 this is an in-process test driving the
//! production tab-strip renderer through `render_tab_bar_to_buffer` (a
//! `TestBackend` wrapper, mirroring `render_button_bar_to_buffer`). No
//! subprocess, no PTY. File-layout-mirrors-catalog (Decision 7): catalog
//! ID `mouse/tabstrip/002`'s presence/absence half lands here with a
//! function name `<sub-area>_<NNN>_<short_suffix>` (Decision 17). The
//! click→close behavior half lives in `tests/e2e_mouse_tabstrip.rs`.
//!
//! M3 contract: Mode and Orchestration tabs carry a clickable `[×]` close
//! affordance; the Dashboard tab (always index 0) carries NONE. The
//! `closeable` mask passed to the renderer encodes that — `false` for the
//! Dashboard tab, `true` for Mode/Orchestration tabs.

use dot_agent_deck::state::SessionStatus;
use dot_agent_deck::ui::render_tab_bar_to_buffer;
use ratatui::style::{Color, Modifier};
use spec::spec;

/// Count the `×` close glyphs in the rendered single-row tab strip.
fn close_glyph_count(buffer: &ratatui::buffer::Buffer) -> usize {
    let area = buffer.area();
    (0..area.width)
        .filter(|&x| buffer[(x, 0)].symbol() == "×")
        .count()
}

/// The foreground color of `label`'s first character in the rendered
/// single-row tab strip. Locates the `" {label} "` padded segment
/// `render_tab_strip` writes for every tab and samples the cell right after
/// the leading pad space, so it survives label reordering / width changes as
/// long as the label text itself is unique in the row.
fn tab_label_fg(buffer: &ratatui::buffer::Buffer, label: &str) -> Color {
    let area = buffer.area();
    let row: String = (0..area.width)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect();
    let needle = format!(" {label} ");
    let start = row
        .find(&needle)
        .unwrap_or_else(|| panic!("label {label:?} not found in rendered tab strip row: {row:?}"));
    // `start` is the leading pad space; the label's own first character sits
    // one column to the right of it.
    buffer[((start + 1) as u16, 0)].fg
}

/// The `Modifier` flags of `label`'s first character in the rendered
/// single-row tab strip. Mirrors `tab_label_fg`'s cell-location logic so the
/// two stay in lockstep; used to assert `REVERSED`/`BOLD` presence/absence,
/// which a plain `fg` check cannot express.
fn tab_label_modifier(buffer: &ratatui::buffer::Buffer, label: &str) -> Modifier {
    let area = buffer.area();
    let row: String = (0..area.width)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect();
    let needle = format!(" {label} ");
    let start = row
        .find(&needle)
        .unwrap_or_else(|| panic!("label {label:?} not found in rendered tab strip row: {row:?}"));
    buffer[((start + 1) as u16, 0)].modifier
}

/// Scenario: Render the tab strip twice. First with only the Dashboard tab
/// (`closeable = [false]`) — the strip must contain NO `×` close glyph,
/// proving the Dashboard tab has no close affordance. Then with Dashboard
/// plus a Mode tab and an Orchestration tab (`closeable = [false, true,
/// true]`) — exactly two `×` glyphs must render, one per closeable tab and
/// none for the Dashboard. RED until M3 renders the `[×]` affordance (today
/// `render_tab_strip` draws no close glyph at all).
#[spec("mouse/tabstrip/002")]
#[test]
fn tabstrip_002_close_glyph_on_mode_orchestration_not_dashboard() {
    // Dashboard alone: never closeable → no close glyph anywhere.
    let dashboard_only = render_tab_bar_to_buffer(&["Dashboard"], &[false], 0, 80, &[None]);
    assert_eq!(
        close_glyph_count(&dashboard_only),
        0,
        "Dashboard tab must render no [×] close affordance, got {:?}",
        dashboard_only_text(&dashboard_only)
    );

    // Dashboard + Mode + Orchestration: only the two non-Dashboard tabs get
    // a close glyph, so exactly two `×` render. A third would mean the
    // Dashboard wrongly gained one; zero means the affordance is missing.
    let three_tabs = render_tab_bar_to_buffer(
        &["Dashboard", "demo", "squad"],
        &[false, true, true],
        0,
        80,
        &[None, None, None],
    );
    assert_eq!(
        close_glyph_count(&three_tabs),
        2,
        "Mode and Orchestration tabs must each render a [×] (and the Dashboard none), got {:?}",
        dashboard_only_text(&three_tabs)
    );
}

/// Stringify the rendered row for assertion messages.
fn dashboard_only_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    (0..area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>()
}

/// Scenario: Render the tab strip with a Dashboard tab plus an orchestration
/// tab whose panes carry a mix of `SessionStatus` values, and assert the
/// orchestration tab's label renders in the `palette::status_color()` of the
/// SINGLE highest-priority status among its panes — fixed order Error(Red) >
/// WaitingForInput(Magenta) > Working(Green) > Thinking/Compacting(Blue) >
/// Idle/Unknown(no tint) (PRD #333). Covers: (a) one `Error` among `Idle`
/// panes -> Red; (b) one `WaitingForInput` among `Working`/`Idle` (no Error)
/// -> Magenta; (c) all `Idle` -> the SAME base label color as an ordinary
/// tab, NOT `Color::DarkGray` (PRD #333 defect B: PRD #13 reserves DarkGray
/// for purely-decorative elements, not label text); (d) `Thinking` +
/// `Working` (no higher-priority state) -> Green, since Working outranks
/// Thinking. Also asserts a non-orchestration tab (`None`) never gets
/// colorized by this feature. RED today on (c): the Idle branch still
/// applies `.fg(palette::status_color(Idle))` (`DarkGray`) instead of
/// falling through to the base style.
#[spec("tabs/orchestration/009")]
#[test]
fn orchestration_009_tab_label_colored_by_highest_priority_status() {
    use SessionStatus::*;

    // (a) Error outranks everything, even surrounded by several Idle panes.
    let buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        0,
        80,
        &[None, Some(&[Idle, Error, Idle])],
    );
    assert_eq!(
        tab_label_fg(&buf, "squad"),
        Color::Red,
        "a tab with an Error pane among Idle panes must render its label Red"
    );

    // (b) No Error present; WaitingForInput outranks Working and Idle.
    let buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        0,
        80,
        &[None, Some(&[Working, WaitingForInput, Idle])],
    );
    // Magenta, not Yellow: issue #579 moved the waiting role off yellow, which
    // measured 1.70:1 against a white terminal background. A tab label is read
    // text, so this is the surface the ratio matters most on
    // (`theme/contrast/002` asserts it).
    assert_eq!(
        tab_label_fg(&buf, "squad"),
        Color::Magenta,
        "a tab with a WaitingForInput pane (no Error present) must render its label Magenta"
    );

    // (c) Every pane Idle -> falls through to the base/unstyled label color,
    // exactly like an ordinary tab. PRD #13 reserves DarkGray for
    // purely-decorative, non-read elements only; a tab label is text, so an
    // all-Idle orchestration tab must NOT be DarkGray (PRD #333 defect B).
    let buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        0,
        80,
        &[None, Some(&[Idle, Idle, Idle])],
    );
    let base_buf = render_tab_bar_to_buffer(
        &["Dashboard", "demo"],
        &[false, false],
        0,
        80,
        &[None, None],
    );
    assert_eq!(
        tab_label_fg(&buf, "squad"),
        tab_label_fg(&base_buf, "demo"),
        "an all-Idle orchestration tab must render with the same base label color as an \
         ordinary tab, not DarkGray"
    );

    // (d) Thinking and Working present, nothing higher-priority -> Green,
    // since Working (priority 3) outranks Thinking (priority 4).
    let buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        0,
        80,
        &[None, Some(&[Thinking, Working])],
    );
    assert_eq!(
        tab_label_fg(&buf, "squad"),
        Color::Green,
        "a tab with Working and Thinking panes (no higher-priority state) must render its \
         label Green — Working outranks Thinking"
    );

    // Non-orchestration tabs are unaffected: a `None` entry must render with
    // the SAME base label color as any other unaffected tab in the same row,
    // never a status color, even though "demo" here also happens to carry no
    // status data.
    let buf = render_tab_bar_to_buffer(
        &["Dashboard", "demo"],
        &[false, false],
        0,
        80,
        &[None, None],
    );
    assert_eq!(
        tab_label_fg(&buf, "Dashboard"),
        tab_label_fg(&buf, "demo"),
        "a non-orchestration tab (None) must render with the same base label color as any \
         other unaffected tab, not a status color"
    );
}

/// Scenario: Render the tab strip with an orchestration tab made the ACTIVE
/// tab (unlike `orchestration_009`, which always leaves Dashboard active) and
/// give it a non-idle (`Error`) pane — assert its label carries NO status
/// `fg` tint and matches an active non-orchestration (Dashboard) tab's `fg`
/// and modifiers exactly (`REVERSED | BOLD`, no absolute color), since
/// stacking a status `fg` on top of `Modifier::REVERSED` inverts the color
/// into a BACKGROUND at display time instead of coloring text (PRD #333
/// defect A — this withdraws the earlier BOLD-without-REVERSED decision).
/// Also asserts an INACTIVE orchestration tab whose aggregate status is
/// `Idle` renders with the same base label color as an ordinary tab, not
/// `Color::DarkGray` (defect B), and that an INACTIVE orchestration tab with
/// a non-idle (`Error`) aggregate status still colors its label text as
/// today (regression guard). RED today: the active branch still applies
/// `.fg(status_color(Error))` on top of `REVERSED`, and the Idle branch
/// still applies `.fg(DarkGray)` instead of falling through to the base
/// style.
#[spec("tabs/orchestration/010")]
#[test]
fn orchestration_010_active_no_status_tint_and_idle_no_grey() {
    use SessionStatus::*;

    // Case 1 (defect A): an ACTIVE orchestration tab with a non-idle (Error)
    // pane must render IDENTICALLY to an active non-orchestration tab — no
    // status fg tint at all, same REVERSED | BOLD modifiers.
    let active_orch_buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        1,
        80,
        &[None, Some(&[Idle, Error, Idle])],
    );
    let active_plain_buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        0,
        80,
        &[None, None],
    );
    assert_eq!(
        tab_label_fg(&active_orch_buf, "squad"),
        tab_label_fg(&active_plain_buf, "Dashboard"),
        "an ACTIVE orchestration tab must carry NO status fg tint — it must render with the \
         same fg as an active non-orchestration tab"
    );
    assert_eq!(
        tab_label_modifier(&active_orch_buf, "squad"),
        tab_label_modifier(&active_plain_buf, "Dashboard"),
        "an ACTIVE orchestration tab must carry the same REVERSED | BOLD modifiers as an \
         active non-orchestration tab"
    );

    // Case 2 (defect B): an INACTIVE orchestration tab whose aggregate
    // status is Idle must render with the base/unstyled label color, not
    // DarkGray — an idle tab must look like an ordinary tab.
    let idle_buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        0,
        80,
        &[None, Some(&[Idle, Idle, Idle])],
    );
    let base_buf = render_tab_bar_to_buffer(
        &["Dashboard", "demo"],
        &[false, false],
        0,
        80,
        &[None, None],
    );
    assert_eq!(
        tab_label_fg(&idle_buf, "squad"),
        tab_label_fg(&base_buf, "demo"),
        "an INACTIVE orchestration tab whose aggregate status is Idle must render with the \
         same base label color as an ordinary tab, not DarkGray"
    );

    // Case 3 (no regression): an INACTIVE orchestration tab with a non-idle
    // (Error) aggregate status still colors its label text, exactly as
    // today, with neither REVERSED nor BOLD.
    let err_buf = render_tab_bar_to_buffer(
        &["Dashboard", "squad"],
        &[false, true],
        0,
        80,
        &[None, Some(&[Idle, Error, Idle])],
    );
    assert_eq!(
        tab_label_fg(&err_buf, "squad"),
        Color::Red,
        "an INACTIVE orchestration tab with an Error pane must still render its label fg Red"
    );
    let inactive_modifier = tab_label_modifier(&err_buf, "squad");
    assert!(
        !inactive_modifier.contains(Modifier::REVERSED)
            && !inactive_modifier.contains(Modifier::BOLD),
        "an inactive orchestration tab must carry neither REVERSED nor BOLD, got {inactive_modifier:?}"
    );
}
