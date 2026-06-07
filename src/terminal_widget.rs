use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

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
}

impl TerminalWidget {
    pub fn new(parser: Arc<Mutex<vt100::Parser>>, title: String, focused: bool) -> Self {
        Self {
            parser,
            title,
            focused,
        }
    }
}

impl Widget for TerminalWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // PRD #13: terminal-relative styling. Focused panes get a Cyan accent
        // border; unfocused panes dim the terminal's own foreground rather than
        // painting an absolute gray. The pane block is left unfilled so the
        // terminal's background shows through (no absolute `terminal_bg` slab).
        let border_style = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
                .fg(Color::Reset)
                .add_modifier(Modifier::DIM)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(self.title);

        let inner = block.inner(area);
        block.render(area, buf);

        let Ok(parser) = self.parser.lock() else {
            return;
        };
        let screen = parser.screen();

        // Render each row of the terminal screen into the inner area.
        // Clamp to the actual PTY dimensions to avoid reading stale cells
        // from a wider/taller buffer before a resize event fires.
        let rows = inner.height as usize;
        let screen_size = screen.size();
        let cols = (inner.width as usize).min(screen_size.1 as usize);

        // Determine which portion of the PTY buffer to display.
        //
        // Use the cursor row as the primary anchor — it reliably marks where
        // the "active" content ends.  A pure last-content-row scan can be
        // fooled by stray characters left over from shell init or escape
        // sequence artifacts, causing the viewport to jump away from the
        // real output.
        //
        // When the cursor is near the top (content fits in the visible area)
        // we show from row 0.  When the cursor is further down we show the
        // window ending at the cursor row.
        let screen_rows = screen_size.0 as usize;
        let cursor_row = screen.cursor_position().0 as usize;
        // The anchor is just past the cursor row (so the cursor line is
        // included), but never beyond the screen height.
        let anchor = (cursor_row + 1).min(screen_rows);
        let effective_rows = anchor.max(rows);
        let start_row = effective_rows.saturating_sub(rows);

        for (y, row_idx) in (start_row..screen_rows).enumerate() {
            if y >= rows {
                break;
            }

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
                y: inner.y + y as u16,
                width: inner.width,
                height: 1,
            };
            Paragraph::new(line).render(line_area, buf);
        }

        // Render a visible block cursor when focused and not scrolled back.
        if self.focused && screen.scrollback() == 0 && !screen.hide_cursor() {
            let cursor_pos = screen.cursor_position();
            let cursor_row = cursor_pos.0 as usize;
            let cursor_col = cursor_pos.1 as usize;

            if cursor_row >= start_row && cursor_row - start_row < rows && cursor_col < cols {
                let cx = inner.x + cursor_col as u16;
                let cy = inner.y + (cursor_row - start_row) as u16;

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
