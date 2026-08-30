use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::palette;
use crate::state::SessionStatus;

/// PRD #84 M5 (invariant 3): a process-wide one-shot guard so the release-mode
/// "PTY size != inner area" fallback logs a single explicit line rather than
/// spamming one per frame. Debug builds trip the `debug_assert!` instead.
static SIZE_MISMATCH_LOGGED: AtomicBool = AtomicBool::new(false);

/// PRD #313 M3: the zoom indicator, appended after the role name in the zoomed
/// pane's border title (`orchestrator [Z]`). tmux marks a zoomed window with a
/// `Z` in its status line; here the border is the only chrome zoom keeps, so the
/// marker rides on it.
///
/// **Bracketed, and AFTER the name — both are load-bearing.** Bracketed so it
/// cannot be confused with a role name, an agent's own output or a card label.
/// After the name because the pane-box scan every orchestration test anchors on
/// (`common::role_pane_left_edge`) looks for `<corner><role>` with no separator
/// — a marker placed before the name would break that scan and take
/// `tabs/orchestration/007` and `e2e_idle_worker_detector.rs` down with it.
///
/// It is rendered as its OWN span, in [`zoom_marker_style`], rather than being
/// concatenated into the title string — see that function for why.
pub(crate) const ZOOM_TITLE_MARKER: &str = "[Z]";

/// PRD #313 M3: the style the [`ZOOM_TITLE_MARKER`] is drawn in — deliberately
/// NOT the title's own style.
///
/// A pane's border title is a *display name*, and display names are agent-
/// reachable: they arrive over the hook socket and are sanitized by
/// `sanitize_display_name`, which strips control characters and bidi overrides
/// but not brackets. So an agent can call itself `worker [Z]` and, as plain
/// title text, make an UNZOOMED pane look zoomed. Rejecting or rewriting such a
/// name is the wrong fix — a user who names a role `worker [Z]` and confuses
/// themselves is not a security boundary, and the sanitizer is shared code — but
/// giving the real indicator a channel that plain text cannot occupy is cheap,
/// and it serves M3's actual goal anyway: an indicator that stands out is better
/// at the job of stopping you from forgetting you are zoomed.
///
/// `REVERSED` rather than a bare colour so the marker is legible on either
/// terminal theme and against whichever border colour the pane's focus/status
/// resolved to (PRD #155 M3), and `BOLD` on top so it survives a terminal that
/// renders reverse video weakly.
pub(crate) fn zoom_marker_style() -> Style {
    Style::default()
        .fg(palette::FOCUSED)
        .add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

/// Converts a vt100 color to a ratatui Color.
fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Converts vt100 cell attributes to a ratatui Style.
fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    style = style.fg(vt100_color_to_ratatui(cell.fgcolor()));
    style = style.bg(vt100_color_to_ratatui(cell.bgcolor()));

    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers |= Modifier::BOLD;
    }
    if cell.italic() {
        modifiers |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifiers |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifiers |= Modifier::REVERSED;
    }

    style = style.add_modifier(modifiers);
    style
}

/// A Ratatui widget that renders a vt100 terminal screen.
///
/// Takes an `Arc<Mutex<vt100::Parser>>` and renders its current screen
/// contents as styled text within the given area.
pub struct TerminalWidget {
    parser: Arc<Mutex<vt100::Parser>>,
    title: String,
    focused: bool,
    /// PRD #155: the agent's status driving the STATUS-aware border (Option A).
    /// When the pane is neither selected nor focused, its border encodes this
    /// status via the centralized [`palette`] — the SAME role the deck card
    /// uses, so a given state looks identical as a deck card and as an embedded
    /// pane. `None` (the [`TerminalWidget::new`] default) keeps the legacy
    /// focus-only behavior: focused → cyan, else a dimmed terminal foreground.
    /// Set it via [`TerminalWidget::with_status`].
    status: Option<SessionStatus>,
    /// Issue #88 follow-up: is the UI in `PaneInput` — i.e. do keystrokes
    /// actually reach this pane right now?
    ///
    /// `focused` alone answers "which pane would receive input", which is NOT
    /// the same question. In command mode the focused pane keeps its focus (it
    /// is where `Ctrl+D` / `Enter` return you) but accepts no keys, and the
    /// Cyan `focused` accent used to render identically in both modes — so the
    /// single loudest border signal claimed "type here" while the keyboard was
    /// driving the deck. The accent is now gated on this flag: colour encodes
    /// *liveness* and border THICKNESS encodes focus, so the two facts stop
    /// competing for one channel.
    ///
    /// Defaults to `true` in [`TerminalWidget::new`] (the pre-gating behaviour),
    /// so a caller that never sets it renders exactly as before. Set it via
    /// [`TerminalWidget::with_input_active`].
    input_active: bool,
    /// PRD #84 M5 (invariant 3): when `true`, the caller attests that the
    /// upstream layout/resize contract held for this pane — i.e. its PTY was
    /// sized to this widget's inner area by `resize_panes_to_layout` earlier in
    /// the same frame — so the widget enforces `screen == inner area` with a
    /// `debug_assert!` (debug) / log-once + min fallback (release). Defaults to
    /// `false` for [`TerminalWidget::new`], so a caller that constructs the
    /// widget directly without that guarantee (e.g. a unit test feeding a
    /// deliberate size mismatch) renders the 1:1 min-from-top fallback without
    /// tripping the contract assert. The production render path opts in via
    /// [`TerminalWidget::contract_guaranteed`].
    contract_guaranteed: bool,
    /// PRD #313 M3: does this pane fill a ZOOMED frame — i.e. is it the one
    /// pane a zoom left on screen? When `true` the border title gains the
    /// [`ZOOM_TITLE_MARKER`], rendered as a separate span in
    /// [`zoom_marker_style`] so it cannot be forged by a display name that
    /// merely contains the same three characters.
    ///
    /// Kept OUT of `title` on purpose. `title` is the pane's name and is what
    /// the PRD #84 contract diagnostic reports; the marker is deck chrome, so
    /// concatenating the two would both style the marker like a name and put
    /// deck state into a diagnostic that is meant to identify a pane.
    ///
    /// Defaults to `false` in [`TerminalWidget::new`]. Set it via
    /// [`TerminalWidget::with_zoom_marker`].
    zoom_marker: bool,
}

impl TerminalWidget {
    pub fn new(parser: Arc<Mutex<vt100::Parser>>, title: String, focused: bool) -> Self {
        Self {
            parser,
            title,
            focused,
            status: None,
            input_active: true,
            contract_guaranteed: false,
            zoom_marker: false,
        }
    }

    /// PRD #313 M3: mark this pane as the one filling a zoomed frame, so its
    /// border title carries the [`ZOOM_TITLE_MARKER`] in [`zoom_marker_style`].
    ///
    /// Pass `true` for the pane the LAYOUT actually expanded this frame — not
    /// merely the focused one. The two normally coincide, but the expanded slot
    /// falls back to pane 0 when nothing in the stack is focused, and a
    /// full-frame pane with no marker is precisely the "you forgot you were
    /// zoomed" failure M3 exists to prevent. See the caller in
    /// `render_terminal_panes`.
    pub fn with_zoom_marker(mut self, zoom_marker: bool) -> Self {
        self.zoom_marker = zoom_marker;
        self
    }

    /// Issue #88 follow-up: tell the widget whether keystrokes currently reach
    /// this pane (`UiMode::PaneInput`). Passing `false` withholds the Cyan
    /// `focused` accent so the border falls through to the agent's STATUS
    /// colour, and thickens the focused pane's border so focus stays legible
    /// without borrowing the colour channel. See the `input_active` field doc.
    pub fn with_input_active(mut self, input_active: bool) -> Self {
        self.input_active = input_active;
        self
    }

    /// PRD #155 (Option A): make the pane border STATUS-aware. When the pane is
    /// neither selected nor focused, its border resolves to `status`'s
    /// centralized [`palette`] role — the SAME color the deck card uses for that
    /// state — so decks and panes stay visually consistent. A focused pane still
    /// takes the `focused` accent (cyan); this only governs the unfocused case.
    /// Non-breaking: callers that don't set a status keep the legacy focus-only
    /// border ([`TerminalWidget::new`] leaves it `None`).
    pub fn with_status(mut self, status: SessionStatus) -> Self {
        self.status = Some(status);
        self
    }

    /// Opt into the PRD #84 invariant-3 contract check (see the field doc).
    ///
    /// HONOR SYSTEM — pass `true` ONLY from a caller that guarantees this pane's
    /// PTY was already sized to this widget's inner area **this frame** (in
    /// practice: the render path, which runs after `resize_panes_to_layout` has
    /// sized every pane it draws from the same `compute_frame_layout`). Passing
    /// `true` anywhere else arms a `debug_assert!` that will spuriously panic in
    /// debug builds whenever the screen size and the area legitimately differ
    /// (e.g. a pane that was not put through the layout/resize pass, or a unit
    /// test feeding a deliberate mismatch). When in doubt, leave it `false`:
    /// the widget still renders correctly via the 1:1 min-from-top fallback.
    pub fn contract_guaranteed(mut self, guaranteed: bool) -> Self {
        self.contract_guaranteed = guaranteed;
        self
    }
}

impl Widget for TerminalWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // PRD #13 + #155: terminal-relative styling, with an Option-A
        // STATUS-aware border resolved through the centralized `palette` (the
        // same precedence the deck card uses). A focused pane gets the `focused`
        // accent (cyan); otherwise, when a status is known, the border encodes
        // that status via the SAME palette role the deck card uses; with no
        // status it falls back to a dimmed terminal foreground. The pane block is
        // left unfilled so the terminal's background shows through (no absolute
        // `terminal_bg` slab).
        //
        // Issue #88 follow-up: the `focused` accent additionally requires
        // `input_active`. Focus and liveness are different facts, and packing
        // both into one colour made command mode invisible — a focused pane
        // looked the same whether or not it was accepting keys. Now colour
        // answers "do my keystrokes land here?" and thickness answers "which
        // pane is focused?", so command mode is legible AND the border reports
        // what every agent is doing while you navigate the deck.
        let border_style = if self.focused && self.input_active {
            Style::default().fg(palette::FOCUSED)
        } else if let Some(status) = self.status.as_ref() {
            Style::default().fg(palette::status_color(status))
        } else {
            Style::default()
                .fg(Color::Reset)
                .add_modifier(Modifier::DIM)
        };

        // Thickness carries focus in command mode, where the colour no longer
        // can. `BorderType` never affects `Block::inner`, so the inner area —
        // and therefore the PTY size and the PRD #84 invariant-3 contract below
        // — is byte-for-byte unchanged by this.
        let border_type = if self.focused && !self.input_active {
            BorderType::Thick
        } else {
            BorderType::Plain
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(border_style)
            // Borrow the title (no per-frame clone): the block is consumed by
            // `block.render` below, releasing the borrow, so `self.title` stays
            // available for the contract-violation diagnostic (PRD #84 M5).
            //
            // PRD #313 M3: while this pane fills a zoomed frame the marker rides
            // after the name as its OWN span, so it is drawn in
            // `zoom_marker_style` and a display name that merely spells `[Z]`
            // renders as ordinary title text beside it.
            .title(if self.zoom_marker {
                Line::from(vec![
                    Span::raw(self.title.as_str()),
                    Span::raw(" "),
                    Span::styled(ZOOM_TITLE_MARKER, zoom_marker_style()),
                ])
            } else {
                Line::from(self.title.as_str())
            });

        let inner = block.inner(area);
        block.render(area, buf);

        let Ok(parser) = self.parser.lock() else {
            return;
        };
        let screen = parser.screen();

        // PRD #84 M5 (invariant 3): render the PTY screen 1:1 against the inner
        // area — screen cell (r, c) maps to inner cell (r, c), row-major, from
        // row 0 / col 0. No `min(area, screen)` column clamp and no
        // cursor-anchored row window: those were defenses against an upstream
        // layout/PTY-size mismatch that `resize_panes_to_layout` (M4) now
        // prevents, and the row window was the direct cause of the "scramble /
        // stale content near the bottom rows" symptom.
        let (screen_rows, screen_cols) = screen.size();

        // Contract guard. When the caller attests the upstream contract held
        // (a layout-sized pane, `contract_guaranteed`), the PTY screen MUST
        // already equal the inner area. A mismatch is a contract violation:
        // loud in debug (`debug_assert!`), and in release a single explicit log
        // plus the safe `min` fallback below — never a panic in production.
        //
        // A zero-dimension inner area is exempt: `resize_panes_to_layout` skips
        // sizing a pane whose target inner area has a zero dimension (its
        // `rows == 0 || cols == 0` guard), so its PTY legitimately keeps its
        // prior size. The `min` fallback below renders nothing into a 0-dim
        // area, so there is nothing to assert.
        if self.contract_guaranteed && inner.height > 0 && inner.width > 0 {
            debug_assert!(
                (screen_rows, screen_cols) == (inner.height, inner.width),
                "PRD #84 invariant 3: a layout-sized pane's PTY screen ({screen_rows}x{screen_cols}) \
                 must equal its inner area ({}x{}); resize_panes_to_layout must size every drawn \
                 pane before render (pane {:?})",
                inner.height,
                inner.width,
                self.title,
            );
            // Release path (the `debug_assert!` above is compiled out): log once
            // so the violation is observable without spamming a line per frame.
            if (screen_rows, screen_cols) != (inner.height, inner.width)
                && !SIZE_MISMATCH_LOGGED.swap(true, Ordering::Relaxed)
            {
                tracing::warn!(
                    screen_rows,
                    screen_cols,
                    inner_height = inner.height,
                    inner_width = inner.width,
                    pane = %self.title,
                    "TerminalWidget: PTY screen size != inner area — falling back to min(area, screen); \
                     see PRD #84 rendering contract (invariant 3)"
                );
            }
        }

        // Fall back to `min(area, screen)` from the top-left so an over- or
        // under-sized screen never reads out of bounds. With the contract held
        // these are exactly the inner dims; the `min` only matters when the
        // contract is violated (release) or for a caller that never attested it.
        let rows = (inner.height as usize).min(screen_rows as usize);
        let cols = (inner.width as usize).min(screen_cols as usize);

        for row_idx in 0..rows {
            let mut spans = Vec::new();
            let mut col = 0;
            let mut run_text = String::new();
            let mut run_style = Style::default();

            while col < cols {
                let cell = screen.cell(row_idx as u16, col as u16);
                let (ch, style) = match cell {
                    Some(cell) => {
                        let c = cell.contents();
                        let s = cell_style(cell);
                        (if c.is_empty() { " " } else { c }, s)
                    }
                    None => (" ", Style::default()),
                };
                if style == run_style && !run_text.is_empty() {
                    run_text.push_str(ch);
                } else {
                    if !run_text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run_text), run_style));
                    }
                    run_text.push_str(ch);
                    run_style = style;
                }
                col += 1;
            }
            if !run_text.is_empty() {
                spans.push(Span::styled(run_text, run_style));
            }

            let line = Line::from(spans);
            let line_area = Rect {
                x: inner.x,
                y: inner.y + row_idx as u16,
                width: inner.width,
                height: 1,
            };
            Paragraph::new(line).render(line_area, buf);
        }

        // Render a visible block cursor when the pane is focused AND live, and
        // not scrolled back. 1:1 mapping: the cursor at screen (row, col) lands
        // at inner (row, col) — no row-window offset now that rendering starts
        // at screen row 0.
        //
        // PRD #341 M1: `input_active` gates this the same way it gates the
        // border accent above. A block cursor is the most universal "your
        // keystrokes land here" affordance a terminal has, and focus
        // deliberately survives the mode switch (`Action::DetachToNormal` keeps
        // the focused pane so Ctrl+D knows where to go back to) — so keying the
        // cursor off `focused` alone painted a live-looking cursor into a pane
        // that accepts nothing, out-loudly contradicting the border. In command
        // mode nothing is painted at all: the PRD's alternative — a dim hollow
        // outline that keeps the cursor's POSITION visible — has no clean
        // expression in one buffer cell, since `REVERSED` renders the same solid
        // block we are removing and `DIM` alone is both invisible on a plain
        // cell and unreliable across terminals. Success criterion: "in command
        // mode, no cursor of any kind renders in the focused pane".
        if self.focused && self.input_active && screen.scrollback() == 0 && !screen.hide_cursor() {
            let cursor_pos = screen.cursor_position();
            let cursor_row = cursor_pos.0 as usize;
            let cursor_col = cursor_pos.1 as usize;

            if cursor_row < rows && cursor_col < cols {
                let cx = inner.x + cursor_col as u16;
                let cy = inner.y + cursor_row as u16;

                if let Some(existing) = buf.cell_mut((cx, cy)) {
                    existing.set_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }
        }
    }
}
